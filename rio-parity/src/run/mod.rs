//! `rio-parity run` — the parity campaign engine.
//!
//! Executes one parity campaign against one eval set: plan the in-scope
//! jobs, sweep cache.nixos.org for Hydra's per-job ground truth,
//! optionally warm upstream-built dependencies, submit batches to the
//! rio cluster, collect and classify outcomes, and render the report.
//! Campaign state is append-only JSONL on the pod volume (periodically
//! synced to S3) so an interrupted run can resume without repeating
//! terminal work.
//!
//! [`run`] is the production entry point (gRPC, S3, and `nix`-child
//! backends); [`run_with_backends`] is the orchestrator proper, taking
//! every external surface behind the [`Backends`] traits so a whole
//! campaign can execute against in-memory fakes (see the end-to-end test
//! in this module). Stages are gated by done-markers in the state
//! directory: plan → hydra-truth → warm (leaf mode) → submit ∥ collect ∥
//! watchdog ∥ sync → report.

pub mod archive_input;
pub mod artifact;
pub mod batch;
pub mod classify;
pub mod collect;
pub mod drv_import;
pub mod evalset_input;
pub mod glob;
pub mod grpc;
pub mod hydra_truth;
pub mod model;
pub mod plan;
pub mod reader;
pub mod report;
pub mod spec;
pub mod state;
pub mod stderrparse;
pub mod submit;
pub mod submitter;
pub mod transport;
pub mod warm;
pub mod watchdog;

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::AtomicU64;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use clap::Args;

use self::artifact::{
    ArtifactStore, S3ArtifactStore, SyncTracker, download_state_if_missing, sync_state,
};
use self::collect::{BatchView, JobContext, process_settled_batch};
use self::grpc::{AdminApi, ClusterApi, GrpcAdminApi, GrpcStoreApi, StoreApi};
use self::hydra_truth::{NarinfoSource, hydra_outcome_for_job};
use self::model::{
    BATCH_KIND_SUBMIT, Bucket, FailureKind, HydraEntry, JobRecord, PauseState, RioOutcome,
    now_rfc3339, rfc3339_to_unix,
};
use self::reader::{GetBuildGraphReader, ResultReader};
use self::spec::{CampaignRecord, CampaignSpec, Knobs, Mode, PlanOutput, generate_campaign_id};
use self::state::{StateDir, StateFile, latest_per_job};
use self::submit::{SubmitTracker, run_submit_loop};
use self::submitter::{NixSubmitter, Submitter};
use self::watchdog::{JobPhase, PollTick, StallKind, StallVerdict, Watchdog};

/// CLI arguments for `rio-parity run`.
#[derive(Debug, Args)]
pub struct RunArgs {
    /// Path to the campaign spec JSON (written by `xtask parity launch`).
    #[arg(long)]
    pub spec: PathBuf,
    /// Local state directory (pod emptyDir). Created if missing.
    #[arg(long, default_value = "./parity-state")]
    pub state_dir: PathBuf,
    /// Local directory containing the downloaded+untarred eval set.
    /// When absent the engine downloads it from S3 per the spec.
    #[arg(long)]
    pub eval_set_dir: Option<PathBuf>,
    /// Override the spec's job limit (smoke runs).
    #[arg(long)]
    pub limit: Option<usize>,
    /// Hard deadline (RFC3339). The engine renders an explicitly-partial
    /// report at the deadline.
    #[arg(long)]
    pub deadline: Option<String>,
    /// Allow running even when the spec does not carry a launch-time
    /// tenant-upstream assertion (the run is flagged low-confidence).
    #[arg(long, default_value_t = false)]
    pub allow_unverified_tenants: bool,
    /// Skip the S3 sync (local development).
    #[arg(long, default_value_t = false)]
    pub no_s3: bool,
}

/// Per-chunk RPC deadline for rio-store `BatchQueryPathInfo` lookups (the
/// plan-time validity snapshot and collect's NAR-identity reads). Each call
/// covers at most [`grpc::BATCH_QUERY_CHUNK`] paths, so one minute is
/// generous headroom over the store's indexed lookup while still failing a
/// wedged connection well inside the collect poll cadence.
const STORE_QUERY_TIMEOUT: Duration = Duration::from_secs(60);

/// Per-path attempt budget for the hydra-truth narinfo sweep (transient
/// cache.nixos.org/Fastly errors only — 404s are recorded, not retried).
const NARINFO_SWEEP_ATTEMPTS: u32 = 5;

/// Rolling window (most recent terminal records) over which the poller
/// computes the infra-failure rate for the backpressure pause.
const INFRA_RATE_WINDOW: usize = 100;

/// Minimum number of terminal records before the rolling infra rate may
/// trigger the backpressure pause, so a tiny early sample cannot pause the
/// campaign on its first unlucky batch.
const INFRA_RATE_MIN_SAMPLE: usize = 20;

/// Emit the poller's progress heartbeat every this many ticks (with the
/// default 60-second poll cadence: roughly every ten minutes), so a long
/// but healthy quiet stretch is distinguishable from a wedged campaign in
/// the logs.
const HEARTBEAT_EVERY_TICKS: u64 = 10;

/// Every external surface the engine touches, behind traits so the whole
/// run can execute against fakes (see the `mini_campaign_end_to_end_and_resume`
/// test).
pub struct Backends {
    pub store: Arc<dyn StoreApi>,
    pub admin: Arc<dyn AdminApi>,
    pub cluster: Arc<dyn ClusterApi>,
    /// Warm-stage read-back only: `run_warm` resolves per-root dispositions
    /// from the build graph. The build-path collect loop reads in-band
    /// per-root results from batches.jsonl instead and never touches this.
    pub reader: Arc<dyn ResultReader>,
    pub submitter: Arc<dyn Submitter>,
    pub narinfo: Arc<dyn NarinfoSource>,
    pub artifacts: Option<Arc<dyn ArtifactStore>>,
}

/// Entry point for `rio-parity run`: load and validate the spec,
/// materialize the eval set on local disk, build the production backends,
/// and hand off to [`run_with_backends`].
///
/// # Operational contract
///
/// - **Pause:** touching `<state-dir>/PAUSE` pauses new submissions
///   (batches already running keep going); removing the file resumes. The
///   file is polled once per watchdog tick
///   (`knobs.cluster_status_poll_secs`, default 60s), so a pause or
///   unpause takes up to one tick to take effect.
/// - **Exit code:** `0` both when the campaign drained completely and when
///   it stopped at the deadline with an explicitly-partial report —
///   consumers must read the `partial` flag in progress.json / summary.md,
///   not the exit code. Non-zero means an error (invalid spec or eval set,
///   unreachable backends, state-dir I/O failure, or a dead background
///   task).
/// - **Deadline:** `--deadline` (or `spec.deadline`) stops *new*
///   submissions once reached; batches already in flight still drain,
///   which can take up to `knobs.batch_timeout_hours`. Do not set a
///   Kubernetes `activeDeadlineSeconds` close to the campaign deadline —
///   it would kill the pod mid-drain instead of letting the partial report
///   render. A supplied deadline that does not parse as RFC3339 is a
///   startup error.
/// - **Image requirements:** GNU `tar` + `zstd` (drv-archive unpack),
///   `nix` and an `ssh` client (the submitter shells out to `nix build`
///   against the gateway's ssh-ng URL), and the per-tenant SSH keys /
///   service HMAC key mounted at the paths named in the spec.
/// - **Resume:** re-running with the same state dir skips completed stages
///   and already-terminal jobs. Resuming on a *fresh* pod volume
///   additionally requires `spec.campaign_id` to be pinned (it names the
///   S3 prefix the synced state is restored from) and S3 to be configured.
/// - **S3 layout:** the eval set is read from
///   `<eval_set.s3_bucket or s3.bucket>/<eval_set.s3_prefix>/…`; campaign
///   artifacts are synced to `<s3.bucket>/<s3.prefix>/<campaign-id>/…`
///   (default prefix `parity/campaigns`).
pub async fn run(args: RunArgs) -> Result<()> {
    let spec = CampaignSpec::load(&args.spec)?;
    let state = StateDir::new(&args.state_dir)?;
    // Campaign artifact store (periodic S3 sync of the state dir).
    let artifacts: Option<Arc<dyn ArtifactStore>> = match (&spec.s3.bucket, args.no_s3) {
        (Some(bucket), false) => Some(Arc::new(S3ArtifactStore::new(bucket.clone()).await)),
        _ => None,
    };
    // The eval set may live in a different bucket than the campaign
    // artifacts; honor `eval_set.s3_bucket` when it names one.
    let eval_store: Option<Arc<dyn ArtifactStore>> = match &spec.eval_set.s3_bucket {
        Some(bucket) if !args.no_s3 && Some(bucket) != spec.s3.bucket.as_ref() => {
            Some(Arc::new(S3ArtifactStore::new(bucket.clone()).await))
        }
        _ => artifacts.clone(),
    };
    // The eval set + drv archive must be on local disk before the backends
    // are built (NixSubmitter needs the archive directory).
    let (eval_dir, archive_dir) = ensure_eval_set(
        &state,
        args.eval_set_dir.clone(),
        &spec,
        eval_store.as_deref(),
    )
    .await?;
    let admin = Arc::new(GrpcAdminApi::new(
        spec.cluster.scheduler_addr.clone(),
        spec.cluster.service_hmac_key_path.as_deref(),
    )?);
    let backends = Backends {
        store: Arc::new(GrpcStoreApi::new(
            spec.cluster.store_addr.clone(),
            STORE_QUERY_TIMEOUT,
        )),
        admin: admin.clone(),
        cluster: admin,
        reader: Arc::new(GetBuildGraphReader::new(GrpcAdminApi::new(
            spec.cluster.scheduler_addr.clone(),
            spec.cluster.service_hmac_key_path.as_deref(),
        )?)),
        submitter: Arc::new(NixSubmitter::new(archive_dir)),
        narinfo: Arc::new(crate::nixcache::NixCacheClient::new(
            &spec.hydra.cache_url,
            &spec.hydra.user_agent,
        )?),
        artifacts,
    };
    run_with_backends(args, spec, state, eval_dir, backends).await
}

/// Locate (or fetch and untar) the eval set. Returns the eval-set
/// directory and the untarred drv-archive directory.
pub async fn ensure_eval_set(
    state: &StateDir,
    explicit_dir: Option<PathBuf>,
    spec: &CampaignSpec,
    artifacts: Option<&dyn ArtifactStore>,
) -> Result<(PathBuf, PathBuf)> {
    let eval_dir = match explicit_dir {
        Some(dir) => dir,
        None => {
            let dest = state.path("evalset");
            std::fs::create_dir_all(&dest)
                .with_context(|| format!("create eval-set dir {}", dest.display()))?;
            let Some(store) = artifacts else {
                bail!("--eval-set-dir not given and no S3 configured to fetch the eval set from");
            };
            let prefix = spec.eval_set.s3_prefix.clone().ok_or_else(|| {
                anyhow::anyhow!(
                    "spec.eval_set.s3_prefix is required when --eval-set-dir is not given"
                )
            })?;
            for name in [
                "evalset.json",
                "manifest.jsonl",
                "dep-closure.jsonl",
                "drvs.tar.zst",
            ] {
                let target = dest.join(name);
                if target.exists() {
                    // Already downloaded by a previous run on this volume; a
                    // torn earlier download is caught loudly downstream (JSON
                    // parse / manifest digest / tar failure), never trusted
                    // silently.
                    continue;
                }
                let key = format!("{prefix}/{name}");
                if let Some(bytes) = store.get_bytes(&key).await? {
                    std::fs::write(&target, bytes)
                        .with_context(|| format!("write {}", target.display()))?;
                } else if name != "dep-closure.jsonl" {
                    bail!("eval set object missing in S3: {key}");
                }
            }
            dest
        }
    };
    // Untar the drv archive once (idempotent: skip when the marker exists).
    let archive_dir = eval_dir.join("drv-archive");
    let tarball = eval_dir.join("drvs.tar.zst");
    if !archive_dir.join(".untarred").exists() {
        if tarball.exists() {
            std::fs::create_dir_all(&archive_dir)
                .with_context(|| format!("create {}", archive_dir.display()))?;
            let status = tokio::process::Command::new("tar")
                .args(["--zstd", "-xf"])
                .arg(&tarball)
                .arg("-C")
                .arg(&archive_dir)
                .kill_on_drop(true)
                .status()
                .await
                .context("spawn tar (the rio-parity image must ship GNU tar + zstd)")?;
            if !status.success() {
                bail!("untar of {} failed with {status}", tarball.display());
            }
            let marker = archive_dir.join(".untarred");
            std::fs::write(&marker, b"ok")
                .with_context(|| format!("write {}", marker.display()))?;
        } else {
            // Local development and tests run without the archive (the
            // FakeSubmitter never imports anything); a real NixSubmitter run
            // against an empty archive fails loudly at the first import.
            std::fs::create_dir_all(&archive_dir)
                .with_context(|| format!("create {}", archive_dir.display()))?;
        }
    }
    Ok((eval_dir, archive_dir))
}

/// Plan-time excluded jobs carry their exclusion as `rio.outcome` with no
/// build/exec fields: write their terminal records right after planning.
/// The exclusion vocabulary equals the bucket names, so both fields are
/// written via [`Bucket::as_str`] (never hand-typed literals).
fn write_plan_time_records(
    state: &StateDir,
    manifest: &[evalset_input::ManifestEntry],
    plan: &PlanOutput,
    mode: &str,
    existing: &BTreeMap<String, JobRecord>,
) -> Result<()> {
    let by_job: HashMap<&str, &evalset_input::ManifestEntry> =
        manifest.iter().map(|m| (m.job.as_str(), m)).collect();
    let emit = |job: &str, bucket: Bucket| -> Result<()> {
        if existing.contains_key(job) {
            return Ok(());
        }
        let Some(m) = by_job.get(job) else {
            return Ok(());
        };
        state.append_jsonl(
            StateFile::Results,
            &JobRecord {
                job: job.to_string(),
                system: m.system.clone(),
                drv_path: m.drv_path.clone(),
                mode: mode.to_string(),
                attempts: 0,
                build_ids: vec![],
                rio: model::RioSide {
                    outcome: bucket.as_str().to_string(),
                    ..Default::default()
                },
                hydra: model::HydraSide {
                    outcome: model::HydraOutcome::Unknown.as_str().to_string(),
                    ..Default::default()
                },
                nar_compare: BTreeMap::new(),
                bucket: bucket.as_str().to_string(),
                cascaded: false,
                signature: None,
                log_key: None,
                repro: String::new(),
                evidence: None,
                updated_at: now_rfc3339(),
            },
        )
    };
    for job in plan.skipped.keys() {
        emit(job, Bucket::Skipped)?;
    }
    for job in &plan.not_attemptable {
        emit(job, Bucket::NotAttemptable)?;
    }
    for job in &plan.cached_prior_jobs {
        emit(job, Bucket::CachedPrior)?;
    }
    Ok(())
}

/// Partial report (deadline/abort): every in-scope job that never reached a
/// record gets an explicit not-attempted [`JobRecord`] (rio outcome and
/// bucket "not-attempted", no build/exec fields), so the report's bucket
/// counts sum to the in-scope total. Returns how many were written.
fn write_not_attempted_records(
    state: &StateDir,
    manifest: &[evalset_input::ManifestEntry],
    plan: &PlanOutput,
    mode: &str,
    existing: &BTreeMap<String, JobRecord>,
) -> Result<usize> {
    let by_job: HashMap<&str, &evalset_input::ManifestEntry> =
        manifest.iter().map(|m| (m.job.as_str(), m)).collect();
    let mut written = 0usize;
    for job in &plan.in_scope {
        if existing.contains_key(job) {
            continue;
        }
        let Some(m) = by_job.get(job.as_str()) else {
            continue;
        };
        state.append_jsonl(
            StateFile::Results,
            &JobRecord {
                job: job.clone(),
                system: m.system.clone(),
                drv_path: m.drv_path.clone(),
                mode: mode.to_string(),
                attempts: 0,
                build_ids: vec![],
                rio: model::RioSide {
                    outcome: RioOutcome::NotAttempted.outcome_str().to_string(),
                    ..Default::default()
                },
                hydra: model::HydraSide {
                    outcome: model::HydraOutcome::Unknown.as_str().to_string(),
                    ..Default::default()
                },
                nar_compare: BTreeMap::new(),
                bucket: Bucket::NotAttempted.as_str().to_string(),
                cascaded: false,
                signature: None,
                log_key: None,
                repro: String::new(),
                evidence: None,
                updated_at: now_rfc3339(),
            },
        )?;
        written += 1;
    }
    Ok(written)
}

/// Jobs whose latest record sits in a terminal bucket.
fn terminal_set(records: &BTreeMap<String, JobRecord>) -> HashSet<String> {
    records
        .iter()
        .filter(|(_, r)| model::is_terminal_bucket(&r.bucket))
        .map(|(job, _)| job.clone())
        .collect()
}

/// Build the submit loop's terminal-set view over the shared results map.
///
/// The submit loop polls this between waves; the view must never *shrink*
/// just because the collect loop happens to hold the results lock at that
/// instant — a transiently empty set would re-offer already-terminal jobs
/// and submit duplicate batches in the late-campaign tail. On lock
/// contention the view returns the last successfully computed snapshot
/// (initially `seed`) instead.
fn terminal_view(
    results: Arc<tokio::sync::Mutex<BTreeMap<String, JobRecord>>>,
    seed: HashSet<String>,
) -> impl Fn() -> HashSet<String> + Send + Sync + 'static {
    let cache = std::sync::Mutex::new(seed);
    move || match results.try_lock() {
        Ok(map) => {
            let fresh = terminal_set(&map);
            *cache
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner) = fresh.clone();
            fresh
        }
        Err(_) => cache
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone(),
    }
}

/// Parse the resolved deadline string (the CLI flag wins over the spec
/// value at the call site). A supplied-but-unparsable deadline is a hard
/// error: the alternative — silently running with no deadline at all —
/// would let a campaign the operator meant to bound overrun its window.
fn parse_deadline(deadline: Option<&str>) -> Result<Option<i64>> {
    match deadline {
        None => Ok(None),
        Some(raw) => match rfc3339_to_unix(raw) {
            Some(unix) => Ok(Some(unix)),
            None => bail!(
                "deadline {raw:?} is not a parseable RFC3339 timestamp \
                 (expected e.g. 2026-06-01T18:00:00Z)"
            ),
        },
    }
}

/// Build the terminal stall record for one job (watchdog escalation →
/// rio-infra-failure with the "stalled-active" / "stalled-queued"
/// signature).
fn stall_record(
    ctx: &JobContext,
    signature: &str,
    mode: &str,
    store_url: &str,
    attempts: u32,
) -> JobRecord {
    let rio_outcome = RioOutcome::TargetFailed {
        kind: FailureKind::Infra,
    };
    JobRecord {
        job: ctx.job.clone(),
        system: ctx.system.clone(),
        drv_path: ctx.drv_path.clone(),
        mode: mode.to_string(),
        attempts,
        build_ids: vec![],
        rio: model::RioSide {
            outcome: rio_outcome.outcome_str().to_string(),
            reason: Some(format!("engine watchdog: {signature}")),
            durations: model::Durations {
                terminal_at: Some(now_rfc3339()),
                ..Default::default()
            },
            ..Default::default()
        },
        hydra: model::HydraSide {
            outcome: ctx.hydra_outcome.as_str().to_string(),
            buildstatus: ctx.hydra_buildstatus,
            outputs: ctx.hydra_outputs.clone(),
        },
        nar_compare: BTreeMap::new(),
        bucket: Bucket::RioInfraFailure.as_str().to_string(),
        cascaded: false,
        signature: Some(signature.to_string()),
        log_key: None,
        repro: submitter::repro_command(store_url, &ctx.drv_path),
        evidence: None,
        updated_at: now_rfc3339(),
    }
}

/// Apply watchdog stall verdicts. The first ActiveStall for a job triggers
/// the single auto-retry: release its in-flight reservation and count an
/// engine resubmission so the submit loop re-offers the job in a fresh
/// batch on the next wave (the stuck batch's nix child is left to the
/// `batch_timeout_hours` backstop — the engine holds no handle to kill it
/// from here, and its eventual settle is harmless under
/// latest-record-wins). A second ActiveStall for the same job, or a
/// QueuedEscalate, writes the terminal rio-infra-failure record
/// ("stalled-active" / "stalled-queued") and retires the job.
/// QueuedRequeue is purely a clock reset (the job is already in the
/// pending set) — log only.
#[allow(clippy::too_many_arguments)]
async fn apply_stall_actions(
    state: &StateDir,
    tracker: &SubmitTracker,
    contexts: &HashMap<String, JobContext>,
    results: &tokio::sync::Mutex<BTreeMap<String, JobRecord>>,
    watchdog: &tokio::sync::Mutex<Watchdog>,
    stall_retries: &mut HashMap<String, u32>,
    stalled: &[StallVerdict],
    mode: &str,
    store_url: &str,
) -> Result<()> {
    for stall in stalled {
        match stall.kind {
            StallKind::QueuedRequeue => {
                tracing::info!(
                    job = %stall.job,
                    requeues_used = stall.requeues_used,
                    "queued watchdog: clock reset, job stays pending (non-terminal re-enqueue)"
                );
            }
            StallKind::ActiveStall => {
                let used = stall_retries.entry(stall.job.clone()).or_insert(0);
                if *used == 0 {
                    *used = 1;
                    tracker.in_flight.lock().await.remove(&stall.job);
                    *tracker
                        .resubmissions
                        .lock()
                        .await
                        .entry(stall.job.clone())
                        .or_default() += 1;
                    tracing::warn!(
                        job = %stall.job,
                        "active stall: single auto-retry — in-flight reservation released, job \
                         re-offered in a fresh batch next wave; the stuck batch runs into the \
                         batch-timeout backstop"
                    );
                } else {
                    write_terminal_stall(
                        state,
                        contexts,
                        results,
                        watchdog,
                        tracker,
                        &stall.job,
                        "stalled-active",
                        mode,
                        store_url,
                    )
                    .await?;
                    tracing::warn!(
                        job = %stall.job,
                        "active stall after the single auto-retry: terminal rio-infra-failure \
                         (stalled-active)"
                    );
                }
            }
            StallKind::QueuedEscalate => {
                write_terminal_stall(
                    state,
                    contexts,
                    results,
                    watchdog,
                    tracker,
                    &stall.job,
                    "stalled-queued",
                    mode,
                    store_url,
                )
                .await?;
                tracing::warn!(
                    job = %stall.job,
                    requeues_used = stall.requeues_used,
                    "queued stall escalation: terminal rio-infra-failure (stalled-queued)"
                );
            }
        }
    }
    Ok(())
}

/// Append the terminal stall record, mirror it into the in-memory results
/// map (so the submit loop's terminal set sees it immediately), and retire
/// the job from the watchdog and the in-flight set.
#[allow(clippy::too_many_arguments)]
async fn write_terminal_stall(
    state: &StateDir,
    contexts: &HashMap<String, JobContext>,
    results: &tokio::sync::Mutex<BTreeMap<String, JobRecord>>,
    watchdog: &tokio::sync::Mutex<Watchdog>,
    tracker: &SubmitTracker,
    job: &str,
    signature: &str,
    mode: &str,
    store_url: &str,
) -> Result<()> {
    let Some(ctx) = contexts.get(job) else {
        tracing::warn!(
            job,
            signature,
            "stall verdict for a job with no context; skipping"
        );
        return Ok(());
    };
    let attempts = tracker.resubmission_count(job).await;
    let record = stall_record(ctx, signature, mode, store_url, attempts);
    state.append_jsonl(StateFile::Results, &record)?;
    results.lock().await.insert(job.to_string(), record);
    watchdog.lock().await.remove_job(job);
    tracker.in_flight.lock().await.remove(job);
    Ok(())
}

/// The orchestrator: plan → hydra-truth → warm → (submit ∥ collect ∥
/// watchdog ∥ sync) → report, with stage done-markers and resume.
pub async fn run_with_backends(
    args: RunArgs,
    spec: CampaignSpec,
    state: StateDir,
    eval_dir: PathBuf,
    backends: Backends,
) -> Result<()> {
    let mut spec = spec;
    if let Some(limit) = args.limit {
        spec.filters.limit = Some(limit);
    }
    // The CLI deadline wins over the spec's; a supplied-but-unparsable value
    // is a startup error rather than a silently unbounded campaign.
    let deadline_raw = args.deadline.clone().or_else(|| spec.deadline.clone());
    let deadline_unix = parse_deadline(deadline_raw.as_deref())?;
    match &deadline_raw {
        Some(raw) => tracing::info!(deadline = %raw, "campaign deadline set"),
        None => tracing::info!("no campaign deadline set; the campaign runs until drained"),
    }
    // Copy-able closure (captures only an Option<i64>) shared by the submit
    // loop and the drain loop.
    let deadline_reached = move || {
        deadline_unix
            .map(|d| jiff::Timestamp::now().as_second() >= d)
            .unwrap_or(false)
    };
    let state = Arc::new(state);

    // ── Resume bootstrap ────────────────────────────────────────────────────
    let existing_campaign: Option<CampaignRecord> = state.read_json("campaign.json")?;
    let campaign_id = existing_campaign
        .as_ref()
        .map(|c| c.campaign_id.clone())
        .or_else(|| spec.campaign_id.clone())
        .unwrap_or_else(|| generate_campaign_id(&now_rfc3339()));
    let artifact_prefix = format!("{}/{}", spec.s3.prefix, campaign_id);
    if let Some(store) = backends.artifacts.as_deref() {
        let restored =
            download_state_if_missing(&state, store, &spec.s3.prefix, &campaign_id).await?;
        if restored {
            tracing::info!(
                campaign_id,
                "restored campaign state from the artifact store"
            );
        }
    }

    // ── Stage: plan ─────────────────────────────────────────────────────────
    let mut campaign = match state.read_json::<CampaignRecord>("campaign.json")? {
        Some(existing) => {
            plan::verify_manifest_digest(&eval_dir, &existing.eval_set.manifest_sha256)?;
            tracing::info!(campaign_id = %existing.campaign_id, "resuming an existing campaign");
            existing
        }
        None => {
            let result = plan::run_plan(
                &spec,
                &eval_dir,
                backends.store.as_ref(),
                args.allow_unverified_tenants,
            )
            .await?;
            let mut record = CampaignRecord::new(
                campaign_id.clone(),
                now_rfc3339(),
                spec.clone(),
                result.pin.clone(),
            );
            record.comparability.low_confidence = result.low_confidence.clone();
            record.plan = Some(result.output.clone());
            state.write_json_atomic("campaign.json", &record)?;
            record
        }
    };
    let plan_output = campaign
        .plan
        .clone()
        .context("campaign.json has no plan output")?;
    let manifest = evalset_input::load_manifest(&eval_dir)?;
    let dep_closure = evalset_input::load_dep_closure(&eval_dir)?;
    let existing_records = latest_per_job(state.load_jsonl(StateFile::Results)?);
    if !state.marker_done("plan") {
        write_plan_time_records(
            &state,
            &manifest,
            &plan_output,
            spec.mode.as_str(),
            &existing_records,
        )?;
        state.set_marker("plan")?;
    }

    // The warm producer map is not persisted in campaign.json — recompute it.
    let warm_comp = plan::compute_warm_sets(&manifest, &dep_closure, &plan_output.in_scope);

    // ── Stage: hydra-truth ──────────────────────────────────────────────────
    let in_scope: HashSet<&str> = plan_output.in_scope.iter().map(String::as_str).collect();
    let target_paths: Vec<String> = manifest
        .iter()
        .filter(|m| in_scope.contains(m.job.as_str()))
        .flat_map(|m| m.outputs.values().cloned())
        .collect();
    if !state.marker_done("hydra-truth") {
        hydra_truth::run_hydra_truth(
            &state,
            backends.narinfo.as_ref(),
            &target_paths,
            &plan_output.warm_set,
            spec.knobs.narinfo_concurrency,
            NARINFO_SWEEP_ATTEMPTS,
        )
        .await?;
        state.set_marker("hydra-truth")?;
    }
    let hydra_by_path: HashMap<String, HydraEntry> = state
        .load_jsonl::<HydraEntry>(StateFile::Hydra)?
        .into_iter()
        .map(|e| (e.path.clone(), e))
        .collect();

    // ── Stage: warm (leaf only) ─────────────────────────────────────────────
    let batch_seq = Arc::new(AtomicU64::new(
        state
            .load_jsonl::<model::BatchRecord>(StateFile::Batches)?
            .iter()
            .map(|b| b.batch_id + 1)
            .max()
            .unwrap_or(1),
    ));
    if spec.mode == Mode::Leaf && !state.marker_done("warm") {
        let warm_url = spec
            .cluster
            .warm_store_url
            .clone()
            .context("leaf mode requires cluster.warm_store_url")?;
        let plan_valid: HashSet<String> = plan_output.cached_prior_paths.iter().cloned().collect();
        warm::run_warm(
            state.clone(),
            backends.submitter.clone(),
            backends.reader.clone(),
            &warm_url,
            &plan_output.warm_set,
            &warm_comp.producer,
            &plan_valid,
            &spec.knobs,
            batch_seq.clone(),
        )
        .await?;
        state.set_marker("warm")?;
    }

    // ── Main loop: submit ∥ collect ∥ watchdog ∥ sync ───────────────────────
    let buildstatus: HashMap<String, i64> = match &spec.hydra.buildstatus_file {
        Some(path) => {
            let text = std::fs::read_to_string(path)
                .with_context(|| format!("read buildstatus file {}", path.display()))?;
            serde_json::from_str(&text)
                .with_context(|| format!("parse buildstatus file {}", path.display()))?
        }
        None => HashMap::new(),
    };
    let job_closures = plan::job_closures(&dep_closure);
    let cached_prior_jobs: HashSet<&str> = plan_output
        .cached_prior_jobs
        .iter()
        .map(String::as_str)
        .collect();
    let not_attemptable: HashSet<&str> = plan_output
        .not_attemptable
        .iter()
        .map(String::as_str)
        .collect();
    let mut contexts: HashMap<String, JobContext> = HashMap::new();
    let mut attemptable: Vec<batch::PendingJob> = Vec::new();
    for m in manifest
        .iter()
        .filter(|m| in_scope.contains(m.job.as_str()))
    {
        let hydra_outcome =
            hydra_outcome_for_job(&m.outputs, &hydra_by_path, buildstatus.get(&m.job).copied());
        let hydra_outputs = m
            .outputs
            .iter()
            .map(|(name, path)| {
                let entry = hydra_by_path.get(path);
                (
                    name.clone(),
                    model::HydraOutput {
                        narinfo_present: entry.map(|e| e.found).unwrap_or(false),
                        nar_hash: entry.and_then(|e| e.nar_hash.clone()),
                        nar_size: entry.and_then(|e| e.nar_size),
                    },
                )
            })
            .collect();
        let (target_drv, dep_drvs) = job_closures
            .get(&m.job)
            .cloned()
            .unwrap_or_else(|| (m.drv_path.clone(), HashSet::new()));
        contexts.insert(
            m.job.clone(),
            JobContext {
                job: m.job.clone(),
                system: m.system.clone(),
                drv_path: m.drv_path.clone(),
                outputs: m.outputs.clone(),
                dep_drvs: dep_drvs.clone(),
                hydra_outcome,
                hydra_outputs,
                hydra_buildstatus: buildstatus.get(&m.job).copied(),
                plan_not_attemptable: not_attemptable.contains(m.job.as_str()),
                plan_snapshot_valid: cached_prior_jobs.contains(m.job.as_str()),
            },
        );
        if !not_attemptable.contains(m.job.as_str()) && !cached_prior_jobs.contains(m.job.as_str())
        {
            attemptable.push(batch::PendingJob {
                job: m.job.clone(),
                drv_path: target_drv,
                dep_drvs: dep_drvs.into_iter().collect(),
            });
        }
    }
    attemptable.sort_by(|a, b| a.job.cmp(&b.job));
    // Owned in-scope membership for the poller's heartbeat (the borrowed
    // `in_scope` set above cannot move into the spawned task).
    let in_scope_jobs: Arc<HashSet<String>> =
        Arc::new(plan_output.in_scope.iter().cloned().collect());

    let pause = Arc::new(PauseState::default());
    let tracker = Arc::new(SubmitTracker::default());
    let results = Arc::new(tokio::sync::Mutex::new(latest_per_job(
        state.load_jsonl(StateFile::Results)?,
    )));
    // Batches already classified by an earlier run (or an earlier pass of
    // this one). Shared between the background collect loop and the drain
    // loop's final passes so no batch is ever processed twice — a double
    // pass would double-count engine resubmissions and could burn the
    // single infra auto-retry budget spuriously.
    let processed: Arc<tokio::sync::Mutex<HashSet<u64>>> = Arc::new(tokio::sync::Mutex::new(
        state
            .read_json::<Vec<u64>>("collected.json")?
            .unwrap_or_default()
            .into_iter()
            .collect(),
    ));
    let watchdog = Arc::new(tokio::sync::Mutex::new(Watchdog::new(spec.knobs.clone())));
    let contexts = Arc::new(contexts);
    let (stop_tx, stop_rx) = tokio::sync::watch::channel(false);

    // Watchdog + backpressure + progress.json + S3 sync poller.
    let poller = {
        let state = state.clone();
        let cluster = backends.cluster.clone();
        let pause = pause.clone();
        let tracker = tracker.clone();
        let results = results.clone();
        let watchdog = watchdog.clone();
        let knobs = spec.knobs.clone();
        let campaign_for_progress = campaign.clone();
        let artifacts = backends.artifacts.clone();
        let prefix = spec.s3.prefix.clone();
        let campaign_id = campaign_id.clone();
        let contexts = contexts.clone();
        let in_scope_jobs = in_scope_jobs.clone();
        let mode = spec.mode.as_str().to_string();
        let store_url = spec.cluster.gateway_store_url.clone();
        let mut stop_rx = stop_rx.clone();
        tokio::spawn(async move {
            let mut sync_tracker = SyncTracker::default();
            let mut ticks: u64 = 0;
            // Per-job count of stall auto-retries already spent (the single
            // auto-retry before stalled-active goes terminal).
            let mut stall_retries: HashMap<String, u32> = HashMap::new();
            let poll_secs = knobs.cluster_status_poll_secs.max(1);
            let ice_every = (knobs.spawn_intents_poll_secs / poll_secs).max(1);
            let sync_every = (knobs.s3_sync_interval_secs / poll_secs).max(1);
            loop {
                if *stop_rx.borrow() {
                    break;
                }
                let cluster_counts = cluster.cluster_status().await.ok();
                let ice = if ticks.is_multiple_of(ice_every) {
                    cluster.spawn_intents().await.ok()
                } else {
                    None
                };
                let manual_pause = state.path("PAUSE").exists();
                pause.set_manual(manual_pause);
                let tick = PollTick {
                    at_unix: jiff::Timestamp::now().as_second(),
                    cluster: cluster_counts.clone(),
                    ice,
                    engine_paused: pause.paused(),
                };
                let outcome = {
                    let mut wd = watchdog.lock().await;
                    // Phase bookkeeping: member of an in-flight batch =
                    // Active, any other non-terminal record = Queued;
                    // terminal jobs are retired from the watchdog.
                    let in_flight = tracker.in_flight.lock().await.clone();
                    let res = results.lock().await;
                    let terminal = terminal_set(&res);
                    for job in res.keys().chain(in_flight.iter()) {
                        if terminal.contains(job) {
                            wd.remove_job(job);
                        } else if in_flight.contains(job) {
                            wd.observe_job(job, JobPhase::Active);
                        } else {
                            wd.observe_job(job, JobPhase::Queued);
                        }
                    }
                    wd.on_tick(&tick)
                };
                // Backpressure: dispatch-gap pause, queue-depth threshold,
                // rolling infra-failure rate.
                let queue_depth_pause = match (knobs.pause_queue_depth, cluster_counts.as_ref()) {
                    (Some(limit), Some(c)) => c.queued_derivations > limit,
                    _ => false,
                };
                let (terminal_in_scope, infra_rate_pct) = {
                    let res = results.lock().await;
                    let mut terminal: Vec<&JobRecord> = res
                        .values()
                        .filter(|r| r.rio.durations.terminal_at.is_some())
                        .collect();
                    terminal.sort_by(|a, b| {
                        b.rio
                            .durations
                            .terminal_at
                            .cmp(&a.rio.durations.terminal_at)
                    });
                    let window: Vec<&&JobRecord> =
                        terminal.iter().take(INFRA_RATE_WINDOW).collect();
                    let infra_rate_pct = if window.len() >= INFRA_RATE_MIN_SAMPLE {
                        let infra = window
                            .iter()
                            .filter(|r| r.bucket == Bucket::RioInfraFailure.as_str())
                            .count();
                        Some((infra as f64 / window.len() as f64) * 100.0)
                    } else {
                        None
                    };
                    let terminal_in_scope = res
                        .iter()
                        .filter(|(job, r)| {
                            in_scope_jobs.contains(job.as_str())
                                && model::is_terminal_bucket(&r.bucket)
                        })
                        .count();
                    (terminal_in_scope, infra_rate_pct)
                };
                let infra_pause = infra_rate_pct.is_some_and(|rate| rate > knobs.infra_pause_pct);
                let backpressure = outcome.dispatch_pause || queue_depth_pause || infra_pause;
                pause.set_backpressure(backpressure);
                // Heartbeat: one info! line on a fixed cadence so a long but
                // healthy quiet stretch is distinguishable from a wedge.
                if ticks.is_multiple_of(HEARTBEAT_EVERY_TICKS) {
                    let in_flight_count = tracker.in_flight.lock().await.len();
                    tracing::info!(
                        terminal_in_scope,
                        in_scope = in_scope_jobs.len(),
                        in_flight = in_flight_count,
                        paused = pause.paused(),
                        manual_pause,
                        dispatch_pause = outcome.dispatch_pause,
                        queue_depth_pause,
                        infra_pause,
                        infra_rate_pct,
                        "campaign heartbeat"
                    );
                }
                if !outcome.stalled.is_empty()
                    && let Err(e) = apply_stall_actions(
                        &state,
                        &tracker,
                        &contexts,
                        &results,
                        &watchdog,
                        &mut stall_retries,
                        &outcome.stalled,
                        &mode,
                        &store_url,
                    )
                    .await
                {
                    tracing::warn!(
                        error = %format!("{e:#}"),
                        "applying stall verdicts failed; retrying on the next poll"
                    );
                }
                // progress.json (atomic rewrite — the status loop polls it)
                // + periodic S3 sync.
                let progress = {
                    let res = results.lock().await;
                    let wd = watchdog.lock().await;
                    report::build_progress(
                        &campaign_for_progress,
                        &res,
                        &wd.suspension_summary(),
                        "submit+collect",
                        now_rfc3339(),
                        None,
                    )
                };
                if let Err(e) = state.write_json_atomic("progress.json", &progress) {
                    tracing::warn!(error = %format!("{e:#}"), "writing progress.json failed");
                }
                if let Some(store) = artifacts.as_deref()
                    && ticks.is_multiple_of(sync_every)
                {
                    match sync_state(&state, store, &prefix, &campaign_id, &mut sync_tracker).await
                    {
                        Ok(uploaded) if uploaded > 0 => {
                            tracing::info!(uploaded, "synced campaign state to the artifact store");
                        }
                        Ok(_) => {}
                        Err(e) => {
                            tracing::warn!(
                                error = %format!("{e:#}"),
                                "state sync failed; retrying on a later tick"
                            );
                        }
                    }
                }
                ticks += 1;
                tokio::select! {
                    _ = tokio::time::sleep(Duration::from_secs(poll_secs)) => {}
                    _ = stop_rx.changed() => break,
                }
            }
        })
    };

    // Background collect loop (timely same-day evidence capture).
    let collector = {
        let state = state.clone();
        let contexts = contexts.clone();
        let tracker = tracker.clone();
        let results = results.clone();
        let processed = processed.clone();
        let knobs = spec.knobs.clone();
        let mode = spec.mode.as_str().to_string();
        let store_url = spec.cluster.gateway_store_url.clone();
        let prefix = artifact_prefix.clone();
        let backends_collect = CollectBackends {
            admin: backends.admin.clone(),
            store: backends.store.clone(),
            artifacts: backends.artifacts.clone(),
        };
        let mut stop_rx = stop_rx.clone();
        tokio::spawn(async move {
            let poll = Duration::from_secs(knobs.collect_poll_secs.max(1));
            loop {
                let pass = {
                    let mut processed_guard = processed.lock().await;
                    collect_pass_with(
                        &state,
                        &backends_collect,
                        &contexts,
                        &tracker,
                        &mut processed_guard,
                        &knobs,
                        &mode,
                        &store_url,
                        Some(&prefix),
                    )
                    .await
                };
                if let Err(e) = pass {
                    tracing::warn!(
                        error = %format!("{e:#}"),
                        "background collect pass failed; retrying on the next poll"
                    );
                }
                match state.load_jsonl(StateFile::Results) {
                    Ok(records) => {
                        let mut res = results.lock().await;
                        *res = latest_per_job(records);
                    }
                    Err(e) => {
                        tracing::warn!(error = %format!("{e:#}"), "reloading results.jsonl failed");
                    }
                }
                if *stop_rx.borrow() {
                    break;
                }
                tokio::select! {
                    _ = tokio::time::sleep(poll) => {}
                    _ = stop_rx.changed() => {}
                }
            }
        })
    };

    // Outer drain loop: submit until drained, run a final synchronous collect
    // pass to catch the tail, and repeat while that pass re-queued work and
    // the deadline has not fired. The body is wrapped so the stop signal and
    // the background-task joins below run on EVERY exit path — success,
    // deadline, or an error mid-loop.
    let drain_result: Result<bool> = async {
        let mut partial = false;
        loop {
            // A background task that stopped on its own can only have
            // panicked (they exit solely on the stop signal): abort the
            // campaign instead of running on with no evidence capture or no
            // watchdog. The join below logs the panic itself.
            if collector.is_finished() {
                bail!(
                    "the background collect task stopped before the campaign finished \
                     (its join error is logged below); aborting the run"
                );
            }
            if poller.is_finished() {
                bail!(
                    "the watchdog/sync poller task stopped before the campaign finished \
                     (its join error is logged below); aborting the run"
                );
            }
            let terminal_seed = terminal_set(&*results.lock().await);
            run_submit_loop(
                state.clone(),
                backends.submitter.clone(),
                tracker.clone(),
                pause.clone(),
                attemptable.clone(),
                terminal_view(results.clone(), terminal_seed),
                deadline_reached,
                spec.cluster.gateway_store_url.clone(),
                spec.knobs.clone(),
                batch_seq.clone(),
            )
            .await?;
            // Final synchronous pass to catch the tail (and any requeues).
            let final_backends = CollectBackends {
                admin: backends.admin.clone(),
                store: backends.store.clone(),
                artifacts: backends.artifacts.clone(),
            };
            let requeued = {
                let mut processed_guard = processed.lock().await;
                collect_pass_with(
                    &state,
                    &final_backends,
                    &contexts,
                    &tracker,
                    &mut processed_guard,
                    &spec.knobs,
                    spec.mode.as_str(),
                    &spec.cluster.gateway_store_url,
                    Some(&artifact_prefix),
                )
                .await?
            };
            {
                let mut res = results.lock().await;
                *res = latest_per_job(state.load_jsonl(StateFile::Results)?);
            }
            if deadline_reached() {
                partial = true;
                break;
            }
            if requeued == 0 {
                break;
            }
        }
        Ok(partial)
    }
    .await;
    // Stop and join the background tasks regardless of how the drain ended;
    // a panicked task is logged here instead of being silently discarded.
    let _ = stop_tx.send(true);
    for (name, handle) in [("collect", collector), ("watchdog/sync poller", poller)] {
        if let Err(e) = handle.await {
            tracing::error!(task = name, error = %e, "background task failed");
        }
    }
    let partial = drain_result?;

    // ── Stage: report ───────────────────────────────────────────────────────
    // Partial run (deadline/abort): backfill explicit not-attempted records
    // for every in-scope job still missing one, so bucket counts sum to
    // in-scope and the partial report is complete over the scope.
    if partial {
        let written = {
            let res = results.lock().await;
            write_not_attempted_records(&state, &manifest, &plan_output, spec.mode.as_str(), &res)?
        };
        if written > 0 {
            tracing::info!(
                written,
                "partial run: backfilled not-attempted records for in-scope jobs without one"
            );
            let mut res = results.lock().await;
            *res = latest_per_job(state.load_jsonl(StateFile::Results)?);
        }
    }
    let final_records: BTreeMap<String, JobRecord> = results.lock().await.clone();
    let suspension = watchdog.lock().await.suspension_summary();
    // Refresh the comparability block in campaign.json with final counts.
    let agg = report::aggregate(&final_records);
    let empty_counts = BTreeMap::new();
    let plan_counts = campaign
        .plan
        .as_ref()
        .map(|p| &p.counts)
        .unwrap_or(&empty_counts);
    campaign.comparability =
        report::comparability_with_counts(&campaign.comparability, &agg, plan_counts);
    state.write_json_atomic("campaign.json", &campaign)?;
    let input = report::ReportInput {
        campaign: &campaign,
        records: &final_records,
        suspension: &suspension,
        generated_at: now_rfc3339(),
        partial,
        top_n: spec.knobs.report_top_n,
    };
    report::write_report(&state, &input)?;
    let progress = report::build_progress(
        &campaign,
        &final_records,
        &suspension,
        "done",
        now_rfc3339(),
        None,
    );
    state.write_json_atomic("progress.json", &progress)?;
    state.set_marker("report")?;
    if let Some(store) = backends.artifacts.as_deref() {
        let mut sync_tracker = SyncTracker::default();
        sync_state(
            &state,
            store,
            &spec.s3.prefix,
            &campaign_id,
            &mut sync_tracker,
        )
        .await?;
    }
    tracing::info!(campaign_id, partial, "campaign run complete");
    Ok(())
}

/// Collect-loop backend bundle (subset of [`Backends`], clonable into the
/// background collect task).
struct CollectBackends {
    admin: Arc<dyn AdminApi>,
    store: Arc<dyn StoreApi>,
    artifacts: Option<Arc<dyn ArtifactStore>>,
}

/// One collect pass over every settled, not-yet-processed submit batch:
/// classify each batch's jobs via [`process_settled_batch`], count an
/// engine resubmission for every re-queued job, and persist the
/// processed-batch set (collected.json) so resume never re-processes a
/// batch. Returns how many job re-queues the pass produced.
#[allow(clippy::too_many_arguments)]
async fn collect_pass_with(
    state: &StateDir,
    backends: &CollectBackends,
    contexts: &HashMap<String, JobContext>,
    tracker: &SubmitTracker,
    processed: &mut HashSet<u64>,
    knobs: &Knobs,
    mode: &str,
    store_url: &str,
    artifact_prefix: Option<&str>,
) -> Result<usize> {
    let batches: Vec<model::BatchRecord> = state.load_jsonl(StateFile::Batches)?;
    let mut requeued = 0usize;
    for batch in batches {
        if batch.kind != BATCH_KIND_SUBMIT || processed.contains(&batch.batch_id) {
            continue;
        }
        let view = BatchView {
            build_id: batch.build_id.clone(),
            results: batch.results.clone(),
            reasons: batch.reasons.clone(),
            engine_cancelled: batch.engine_cancelled,
            submitted_at: Some(batch.started_at.clone()),
        };
        // prior_requeues carries each job's TOTAL engine resubmission count
        // so far — any prior requeue consumes the single infra auto-retry
        // budget (see `collect::decide`).
        let prior_requeues: HashMap<String, u32> = {
            let resubs = tracker.resubmissions.lock().await;
            batch
                .jobs
                .iter()
                .map(|j| (j.clone(), *resubs.get(j).unwrap_or(&0)))
                .collect()
        };
        // first_active_at: approximation — the batch's started_at (the job
        // became Active when its batch went in flight); the in-band per-root
        // results carry scheduler-side start/stop times, not a first-active
        // timestamp.
        let first_active: HashMap<String, String> = batch
            .jobs
            .iter()
            .map(|j| (j.clone(), batch.started_at.clone()))
            .collect();
        let artifacts_pair = backends
            .artifacts
            .as_deref()
            .zip(artifact_prefix.map(String::from));
        let requeue = process_settled_batch(
            state,
            backends.admin.as_ref(),
            backends.store.as_ref(),
            artifacts_pair,
            contexts,
            &batch.jobs,
            &view,
            &prior_requeues,
            knobs,
            mode,
            store_url,
            &first_active,
        )
        .await?;
        {
            let mut resubs = tracker.resubmissions.lock().await;
            for job in &requeue {
                *resubs.entry(job.clone()).or_default() += 1;
                requeued += 1;
            }
        }
        processed.insert(batch.batch_id);
        let mut done: Vec<u64> = processed.iter().copied().collect();
        done.sort_unstable();
        state.write_json_atomic("collected.json", &done)?;
    }
    Ok(requeued)
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::path::Path;

    use async_trait::async_trait;

    use crate::run::evalset_input::test_fixtures::write_mini_eval_set;
    use crate::run::grpc::test_support::FakeStoreApi;
    use crate::run::grpc::{ClusterCounts, GraphSnapshot, IceSnapshot, PoisonedView};
    use crate::run::model::{
        DISPOSITION_NOT_FOUND_UPSTREAM, DISPOSITION_SUBSTITUTED, HydraOutcome, PathOutcome,
        STATUS_COMPLETED, WarmEntry, build_status_name,
    };
    use crate::run::reader::DrvObservation;
    use crate::run::reader::test_support::FakeReader;
    use crate::run::submitter::BatchOutcome;
    use crate::run::submitter::test_support::FakeSubmitter;
    use rio_nix::protocol::build::BuildStatus;

    struct HealthyCluster;
    #[async_trait]
    impl ClusterApi for HealthyCluster {
        async fn cluster_status(&self) -> Result<ClusterCounts> {
            Ok(ClusterCounts {
                active_executors: 2,
                queued_derivations: 1,
                running_derivations: 1,
                substituting_derivations: 0,
            })
        }
        async fn spawn_intents(&self) -> Result<IceSnapshot> {
            Ok(IceSnapshot::default())
        }
    }

    struct NoLogsAdmin;
    #[async_trait]
    impl AdminApi for NoLogsAdmin {
        async fn get_build_graph(&self, _build_id: &str) -> Result<GraphSnapshot> {
            Ok(GraphSnapshot::default())
        }
        async fn list_poisoned(&self) -> Result<Vec<PoisonedView>> {
            Ok(vec![])
        }
        async fn log_tail(&self, _drv: &str, _exec: Option<&str>, _max: usize) -> Result<Vec<u8>> {
            Ok(Vec::new())
        }
        async fn list_builds(
            &self,
            _tenant: &str,
            _limit: u32,
        ) -> Result<Vec<(String, Option<String>)>> {
            Ok(vec![])
        }
    }

    /// In-memory narinfo source keyed by FULL store path — the
    /// [`NarinfoSource`] trait receives the full path (the production
    /// client extracts the hash part itself).
    struct MapNarinfo(HashMap<String, String>);
    #[async_trait]
    impl NarinfoSource for MapNarinfo {
        async fn fetch_narinfo_text(&self, store_path: &str) -> Result<Option<String>> {
            Ok(self.0.get(store_path).cloned())
        }
    }

    fn narinfo_text(path: &str) -> String {
        format!(
            "StorePath: {path}\nURL: nar/x.nar.zst\nCompression: zstd\nNarHash: sha256:{}\nNarSize: 10\nReferences: \n",
            "0".repeat(52)
        )
    }

    fn leaf_spec() -> CampaignSpec {
        let mut spec: CampaignSpec = serde_json::from_str(
            r#"{
              "campaign_id": "c-e2e",
              "mode": "leaf",
              "eval_set": {"hydra_eval_id": 1824219, "key_digest": "deadbeef"},
              "cluster": {"gateway_store_url": "ssh-ng://rio@gw:22?ssh-key=/k",
                          "warm_store_url": "ssh-ng://rio@gw:22?ssh-key=/w",
                          "scheduler_addr": "s:9001", "store_addr": "st:9002"},
              "tenants": {"build_tenant": "parity-leaf", "warm_tenant": "parity-warm",
                          "upstreams_verified": true},
              "filters": {"systems": ["x86_64-linux"], "exclude_features": ["kvm"]},
              "s3": {"prefix": "parity/campaigns"}
            }"#,
        )
        .unwrap();
        // Tight loop intervals so the end-to-end test finishes fast.
        spec.knobs.collect_poll_secs = 1;
        spec.knobs.cluster_status_poll_secs = 1;
        spec.knobs.s3_sync_interval_secs = 1;
        spec
    }

    fn run_args(state_dir: &Path, eval_dir: &Path) -> RunArgs {
        RunArgs {
            spec: PathBuf::from("/dev/null"),
            state_dir: state_dir.to_path_buf(),
            eval_set_dir: Some(eval_dir.to_path_buf()),
            limit: None,
            deadline: None,
            allow_unverified_tenants: false,
            no_s3: true,
        }
    }

    /// Minimal terminal (match-built) record for tests that only need a
    /// terminal bucket, not full evidence.
    fn terminal_record(job: &str) -> JobRecord {
        JobRecord {
            job: job.into(),
            system: "x86_64-linux".into(),
            drv_path: format!("/nix/store/{}-x.drv", "a".repeat(32)),
            mode: "leaf".into(),
            attempts: 1,
            build_ids: vec![],
            rio: model::RioSide::default(),
            hydra: model::HydraSide::default(),
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

    #[test]
    fn malformed_deadline_is_rejected_naming_the_value() {
        assert_eq!(parse_deadline(None).unwrap(), None);
        assert!(
            parse_deadline(Some("2026-06-01T18:00:00Z"))
                .unwrap()
                .is_some()
        );
        let err = parse_deadline(Some("tomorrow-ish")).unwrap_err();
        assert!(err.to_string().contains("tomorrow-ish"), "{err}");
    }

    /// The submit loop's terminal view must never shrink to empty just
    /// because the results lock is momentarily held by the collect loop:
    /// under contention it returns the last computed snapshot instead.
    #[tokio::test]
    async fn terminal_view_returns_last_snapshot_under_lock_contention() {
        let mut map = BTreeMap::new();
        map.insert(
            "done.x86_64-linux".to_string(),
            terminal_record("done.x86_64-linux"),
        );
        let results = Arc::new(tokio::sync::Mutex::new(map));
        // Seeded empty: the first uncontended call computes and caches the
        // live set.
        let view = terminal_view(results.clone(), HashSet::new());
        let live: HashSet<String> = ["done.x86_64-linux".to_string()].into();
        assert_eq!(view(), live);
        // Contended: another task holds the results lock — the view must
        // return the cached snapshot, not an empty set.
        let _guard = results.lock().await;
        assert_eq!(view(), live);
    }

    /// The synthetic stdenv dependency output of `job` from the mini eval
    /// set's dep-closure.jsonl (the dep that is NOT libA's own output).
    fn stdenv_dep_path(eval_dir: &Path, job: &str) -> String {
        let entries = evalset_input::load_dep_closure(eval_dir).unwrap();
        let entry = entries.iter().find(|d| d.job == job).unwrap();
        entry
            .deps
            .iter()
            .flat_map(|d| d.output_paths.clone())
            .find(|p| p.contains("stdenv"))
            .expect("mini eval set has a stdenv dep")
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn mini_campaign_end_to_end_and_resume() {
        let eval_dir = tempfile::tempdir().unwrap();
        write_mini_eval_set(eval_dir.path());
        let manifest = evalset_input::load_manifest(eval_dir.path()).unwrap();
        let app = manifest
            .iter()
            .find(|m| m.job == "appB.x86_64-linux")
            .unwrap()
            .clone();
        let lib = manifest
            .iter()
            .find(|m| m.job == "libA.x86_64-linux")
            .unwrap()
            .clone();
        let lib_out = lib.outputs["out"].clone();
        let stdenv_out = stdenv_dep_path(eval_dir.path(), &app.job);

        // Hydra: appB's outputs + libA's out exist upstream; the stdenv dep
        // does not (its narinfo lookup misses → not-found-upstream).
        let mut narinfos = HashMap::new();
        for p in app.outputs.values() {
            narinfos.insert(p.clone(), narinfo_text(p));
        }
        narinfos.insert(lib_out.clone(), narinfo_text(&lib_out));

        // Submitter script: the warm batch settles first, then the appB
        // submit batch. FakeSubmitter pops from the BACK: push the SUBMIT
        // outcome first. The submit batch carries appB's terminal outcome
        // in band (per-root results); only the warm stage still reads its
        // dispositions back through the reader.
        let submitter = Arc::new(FakeSubmitter::default());
        let warm_build = "0193e4a2-7c1b-7d20-9b3a-00000000aaaa";
        let submit_build = "0193e4a2-7c1b-7d20-9b3a-00000000bbbb";
        submitter.outcomes.lock().unwrap().push(Ok(BatchOutcome {
            build_id: Some(submit_build.into()),
            exit_code: Some(0),
            results: vec![PathOutcome {
                drv_path: app.drv_path.clone(),
                status: build_status_name(BuildStatus::Built).into(),
                error_msg: String::new(),
                start_time: 0,
                stop_time: 0,
            }],
            ..BatchOutcome::default()
        }));
        submitter.outcomes.lock().unwrap().push(Ok(BatchOutcome {
            build_id: Some(warm_build.into()),
            exit_code: Some(0),
            ..BatchOutcome::default()
        }));
        let reader = Arc::new(FakeReader::default());
        reader.set(
            warm_build,
            DrvObservation {
                drv_path: lib.drv_path.clone(),
                status: STATUS_COMPLETED.into(),
                ..DrvObservation::default()
            },
        );

        // Empty rio-store: nothing is valid at the plan snapshot (so appB is
        // attemptable, not cached-prior); collect's NAR read then finds no
        // info and records the outputs without hashes (not-comparable),
        // which does not affect the bucket.
        let state_dir = tempfile::tempdir().unwrap();
        let backends = || Backends {
            store: Arc::new(FakeStoreApi::default()),
            admin: Arc::new(NoLogsAdmin),
            cluster: Arc::new(HealthyCluster),
            reader: reader.clone(),
            submitter: submitter.clone(),
            narinfo: Arc::new(MapNarinfo(narinfos.clone())),
            artifacts: None,
        };
        let state = StateDir::new(state_dir.path()).unwrap();
        run_with_backends(
            run_args(state_dir.path(), eval_dir.path()),
            leaf_spec(),
            state,
            eval_dir.path().to_path_buf(),
            backends(),
        )
        .await
        .unwrap();

        // Final state assertions.
        let state = StateDir::new(state_dir.path()).unwrap();
        for marker in ["plan", "hydra-truth", "warm", "report"] {
            assert!(state.marker_done(marker), "marker {marker} set");
        }
        let records = latest_per_job(state.load_jsonl(StateFile::Results).unwrap());
        assert_eq!(records["appB.x86_64-linux"].bucket, "match-built");
        assert_eq!(records["libA.x86_64-linux"].bucket, "not-attemptable");
        assert_eq!(records["kvmTest.x86_64-linux"].bucket, "skipped");
        assert_eq!(records["libA.aarch64-linux"].bucket, "skipped");
        let warm_entries: Vec<WarmEntry> = state.load_jsonl(StateFile::Warm).unwrap();
        let by_path: HashMap<String, String> = warm_entries
            .iter()
            .map(|w| (w.path.clone(), w.disposition.clone()))
            .collect();
        assert_eq!(by_path[&lib_out], DISPOSITION_SUBSTITUTED);
        assert_eq!(by_path[&stdenv_out], DISPOSITION_NOT_FOUND_UPSTREAM);
        let summary = std::fs::read_to_string(state.path("report/summary.md")).unwrap();
        assert!(summary.contains("Build-outcome parity"));
        assert!(state.path("buckets/match-built.jsonl").exists());
        let progress: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(state.path("progress.json")).unwrap())
                .unwrap();
        assert_eq!(progress["stage"], "done");

        // Resume: same state dir, no scripted submitter outcomes left → must
        // not submit anything new and must finish with identical buckets.
        let submitted_before = submitter.submitted.lock().unwrap().len();
        let state2 = StateDir::new(state_dir.path()).unwrap();
        run_with_backends(
            run_args(state_dir.path(), eval_dir.path()),
            leaf_spec(),
            state2,
            eval_dir.path().to_path_buf(),
            backends(),
        )
        .await
        .unwrap();
        assert_eq!(
            submitter.submitted.lock().unwrap().len(),
            submitted_before,
            "resume submits nothing"
        );
        let records2 = latest_per_job(
            StateDir::new(state_dir.path())
                .unwrap()
                .load_jsonl(StateFile::Results)
                .unwrap(),
        );
        assert_eq!(records2["appB.x86_64-linux"].bucket, "match-built");
    }

    /// Drives the watchdog with a fake clock (tick timestamps) through the
    /// stall-action policy: first ActiveStall → single auto-retry (in-flight
    /// reservation released, resubmission counted, no record); second
    /// ActiveStall for the same job → terminal stalled-active;
    /// QueuedEscalate → terminal stalled-queued.
    #[tokio::test]
    async fn stall_actions_retry_then_terminal() {
        let dir = tempfile::tempdir().unwrap();
        let state = StateDir::new(dir.path()).unwrap();
        let tracker = SubmitTracker::default();
        let results = tokio::sync::Mutex::new(BTreeMap::new());
        // Default knobs: active stall 6h, queued watchdog 2h.
        let wd = tokio::sync::Mutex::new(Watchdog::new(Knobs::default()));
        let mut stall_retries: HashMap<String, u32> = HashMap::new();

        let active_job = "appB.x86_64-linux";
        let queued_job = "libA.x86_64-linux";
        let mk_ctx = |job: &str, drv: &str| JobContext {
            job: job.to_string(),
            system: "x86_64-linux".into(),
            drv_path: drv.to_string(),
            outputs: BTreeMap::new(),
            dep_drvs: HashSet::new(),
            hydra_outcome: HydraOutcome::Built,
            hydra_outputs: BTreeMap::new(),
            hydra_buildstatus: None,
            plan_not_attemptable: false,
            plan_snapshot_valid: false,
        };
        let mut contexts = HashMap::new();
        contexts.insert(
            active_job.to_string(),
            mk_ctx(
                active_job,
                "/nix/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-appB.drv",
            ),
        );
        contexts.insert(
            queued_job.to_string(),
            mk_ctx(
                queued_job,
                "/nix/store/bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb-libA.drv",
            ),
        );
        // The active job sits in an in-flight batch.
        tracker
            .in_flight
            .lock()
            .await
            .insert(active_job.to_string());

        let healthy = ClusterCounts {
            active_executors: 8,
            queued_derivations: 5,
            running_derivations: 5,
            substituting_derivations: 0,
        };
        let tick_at = |at: i64| PollTick {
            at_unix: at,
            cluster: Some(healthy.clone()),
            ice: None,
            engine_paused: false,
        };

        // Fake clock: baseline tick at t=0, then a tick 7h later (healthy,
        // so the whole delta accrues) → first ActiveStall.
        let first = {
            let mut wd = wd.lock().await;
            wd.observe_job(active_job, JobPhase::Active);
            wd.on_tick(&tick_at(0));
            wd.on_tick(&tick_at(7 * 3600))
        };
        assert!(
            first
                .stalled
                .iter()
                .any(|s| s.job == active_job && s.kind == StallKind::ActiveStall)
        );
        apply_stall_actions(
            &state,
            &tracker,
            &contexts,
            &results,
            &wd,
            &mut stall_retries,
            &first.stalled,
            "leaf",
            "ssh-ng://gw",
        )
        .await
        .unwrap();
        // Auto-retry effects: reservation released, resubmission counted, no
        // terminal record yet.
        assert!(!tracker.in_flight.lock().await.contains(active_job));
        assert_eq!(tracker.resubmission_count(active_job).await, 1);
        assert!(
            state
                .load_jsonl::<JobRecord>(StateFile::Results)
                .unwrap()
                .is_empty()
        );

        // The retry goes back in flight; the fake clock advances another 7h
        // of healthy time → second ActiveStall → terminal stalled-active.
        tracker
            .in_flight
            .lock()
            .await
            .insert(active_job.to_string());
        let second = {
            let mut wd = wd.lock().await;
            wd.observe_job(active_job, JobPhase::Active);
            wd.on_tick(&tick_at(14 * 3600))
        };
        assert!(
            second
                .stalled
                .iter()
                .any(|s| s.job == active_job && s.kind == StallKind::ActiveStall)
        );
        apply_stall_actions(
            &state,
            &tracker,
            &contexts,
            &results,
            &wd,
            &mut stall_retries,
            &second.stalled,
            "leaf",
            "ssh-ng://gw",
        )
        .await
        .unwrap();
        let records = latest_per_job(state.load_jsonl(StateFile::Results).unwrap());
        assert_eq!(records[active_job].bucket, "rio-infra-failure");
        assert_eq!(
            records[active_job].signature.as_deref(),
            Some("stalled-active")
        );
        assert!(
            results.lock().await.contains_key(active_job),
            "in-memory results updated"
        );
        assert!(!tracker.in_flight.lock().await.contains(active_job));

        // QueuedEscalate goes terminal immediately with the stalled-queued
        // signature.
        let escalate = vec![StallVerdict {
            job: queued_job.to_string(),
            kind: StallKind::QueuedEscalate,
            requeues_used: 2,
        }];
        apply_stall_actions(
            &state,
            &tracker,
            &contexts,
            &results,
            &wd,
            &mut stall_retries,
            &escalate,
            "leaf",
            "ssh-ng://gw",
        )
        .await
        .unwrap();
        let records = latest_per_job(state.load_jsonl(StateFile::Results).unwrap());
        assert_eq!(records[queued_job].bucket, "rio-infra-failure");
        assert_eq!(
            records[queued_job].signature.as_deref(),
            Some("stalled-queued")
        );
    }

    /// Deadline-partial path: write_not_attempted_records backfills every
    /// in-scope job with no record yet, so bucket counts sum to in-scope.
    #[test]
    fn partial_report_backfills_not_attempted_to_in_scope_total() {
        let eval_dir = tempfile::tempdir().unwrap();
        write_mini_eval_set(eval_dir.path());
        let manifest = evalset_input::load_manifest(eval_dir.path()).unwrap();
        let state_dir = tempfile::tempdir().unwrap();
        let state = StateDir::new(state_dir.path()).unwrap();

        // In scope: appB + libA. libA already has a terminal plan-time
        // record (not-attemptable); appB never produced one (deadline hit
        // first).
        let plan = PlanOutput {
            in_scope: vec!["appB.x86_64-linux".into(), "libA.x86_64-linux".into()],
            ..PlanOutput::default()
        };
        let lib = manifest
            .iter()
            .find(|m| m.job == "libA.x86_64-linux")
            .unwrap();
        state
            .append_jsonl(
                StateFile::Results,
                &JobRecord {
                    job: lib.job.clone(),
                    system: lib.system.clone(),
                    drv_path: lib.drv_path.clone(),
                    mode: "leaf".into(),
                    attempts: 0,
                    build_ids: vec![],
                    rio: model::RioSide {
                        outcome: Bucket::NotAttemptable.as_str().into(),
                        ..Default::default()
                    },
                    hydra: model::HydraSide {
                        outcome: HydraOutcome::Unknown.as_str().into(),
                        ..Default::default()
                    },
                    nar_compare: BTreeMap::new(),
                    bucket: Bucket::NotAttemptable.as_str().into(),
                    cascaded: false,
                    signature: None,
                    log_key: None,
                    repro: String::new(),
                    evidence: None,
                    updated_at: now_rfc3339(),
                },
            )
            .unwrap();

        let existing = latest_per_job(state.load_jsonl(StateFile::Results).unwrap());
        let written =
            write_not_attempted_records(&state, &manifest, &plan, "leaf", &existing).unwrap();
        assert_eq!(
            written, 1,
            "only the record-less attemptable job is backfilled"
        );

        let records = latest_per_job(state.load_jsonl(StateFile::Results).unwrap());
        let app = &records["appB.x86_64-linux"];
        assert_eq!(app.bucket, "not-attempted");
        assert_eq!(app.rio.outcome, "not-attempted");
        assert!(app.build_ids.is_empty() && app.rio.exec_id.is_none());
        // Bucket counts sum to in-scope: the partial report is complete over
        // the scope.
        let agg = report::aggregate(&records);
        assert_eq!(
            agg.bucket_counts.values().sum::<usize>(),
            plan.in_scope.len()
        );
        // Idempotent: nothing more to write on a second call.
        assert_eq!(
            write_not_attempted_records(&state, &manifest, &plan, "leaf", &records).unwrap(),
            0
        );
    }
}
