//! `rio-replay run` — the build-replay campaign engine.
//!
//! Executes one replay campaign against one replay archive: plan the
//! in-scope jobs, load each job's expected outcome from the archive's
//! recorded truth, supply dependencies to the target (scheduler-side
//! prefetch and client uploads), submit the workload to the rio cluster —
//! queue-driven batches in timeless mode, or the recorded request schedule
//! via the timed dispatcher when `spec.scheduling.mode` is timed — collect
//! and classify outcomes, and render the report. Campaign state is
//! append-only JSONL on the pod volume (periodically synced to S3) so an
//! interrupted run can resume without repeating terminal work.
//!
//! [`run`] is the production entry point (gRPC, S3, and gateway
//! worker-protocol backends); [`run_with_backends`] is the orchestrator
//! proper, taking every external surface behind the [`Backends`] traits so
//! a whole campaign can execute against in-memory fakes (see the
//! end-to-end test in this module). Stages are gated by done-markers in
//! the state directory: plan → truth load → supply →
//! execute ∥ collect ∥ watchdog ∥ sync → report.

pub mod archive_input;
pub mod artifact;
pub mod batch;
pub mod classify;
pub mod collect;
pub mod drv_import;
pub mod glob;
pub mod grpc;
pub mod model;
pub mod plan;
pub mod report;
pub mod spec;
pub mod state;
pub mod stderrparse;
pub mod submit;
pub mod submitter;
pub mod supply;
pub mod timeline;
pub mod transport;
pub mod truth;
pub mod watchdog;

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use clap::Args;
use sha2::Digest as _;

use crate::archive::reader::ReplayArchive;
use crate::archive::s3::{ARCHIVE_IMAGE_OBJECT, ARCHIVE_MANIFEST_OBJECT, CompleteMarker};
use crate::archive::schema::{ExpectedOutcome, MemberDigest, OutcomeRecord, RequestRecord};

use self::artifact::{
    ArtifactStore, S3ArtifactStore, SyncTracker, download_state_if_missing, sync_state,
};
use self::collect::{BatchView, JobContext, process_settled_batch};
use self::grpc::{AdminApi, ClusterApi, GrpcAdminApi, GrpcStoreApi, StoreApi};
use self::model::{
    BATCH_KIND_SUBMIT, BATCH_KIND_TIMED, Disposition, FailureKind, JobRecord, PauseState,
    RioOutcome, SupplyEntry, Verdict, now_rfc3339, rfc3339_to_unix,
};
use self::spec::{
    CampaignRecord, CampaignSpec, Knobs, Mode, PLAN_COUNT_RSS_BEFORE, PLAN_COUNT_RSS_PEAK,
    PlanOutput, ScheduleMode, SupplyDelivery, SupplyDependencies, generate_campaign_id,
};
use self::state::{StateDir, StateFile, latest_per_job};
use self::submit::{SubmitTracker, run_submit_loop};
use self::submitter::{ClientOpsSubmitter, Submitter};
use self::supply::exec::{
    LadderTopup, PoolSupplyTransport, PreSubmitSupply, SupplyInputs, SupplyStageReport,
    SupplyTransport, refresh_outcome_counts, run_supply_stage,
};
use self::timeline::{
    RecordedRequest, RecordedTarget, RecordedTiming, ScheduledRequest, SharedTimingLookup,
    TimedRunStats, TimelineConfig, build_schedule, run_timed_dispatch,
};
use self::truth::NarinfoSource;
use self::watchdog::{
    COMPONENT_DISPATCH, COMPONENT_PAUSE, JobPhase, PollTick, StallKind, StallVerdict, Watchdog,
};

/// CLI arguments for `rio-replay run`.
#[derive(Debug, Args)]
pub struct RunArgs {
    /// Path to the campaign spec JSON (written by `xtask replay launch`).
    #[arg(long)]
    pub spec: PathBuf,
    /// Local state directory (pod emptyDir). Created if missing.
    #[arg(long, default_value = "./replay-state")]
    pub state_dir: PathBuf,
    /// Local replay archive (a .dwarfs image or a directory-form archive);
    /// skips the S3 fetch.
    #[arg(long)]
    pub archive: Option<PathBuf>,
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

/// Per-path attempt budget for the warm-set upstream-coverage probe
/// (transient upstream/CDN errors only — 404s are recorded, not retried).
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

/// Upper bound (one year, in seconds) on recorded request offsets and stop
/// offsets accepted from an archive. No real recording window comes close;
/// the cap exists so corrupt or absurd recorded values can neither panic
/// the schedule's duration math nor park a request unreachably far in the
/// future.
const MAX_RECORDED_OFFSET_S: f64 = 365.0 * 24.0 * 3600.0;

/// Every external surface the engine touches, behind traits so the whole
/// run can execute against fakes (see the `mini_campaign_end_to_end_and_resume`
/// test).
pub struct Backends {
    pub store: Arc<dyn StoreApi>,
    pub admin: Arc<dyn AdminApi>,
    pub cluster: Arc<dyn ClusterApi>,
    /// Build-path submitter (the worker-protocol client-ops transport in
    /// production).
    pub submitter: Arc<dyn Submitter>,
    /// Supply-stage transport (validity probes, client uploads, prefetch
    /// builds). `None` = construct the production [`PoolSupplyTransport`]
    /// from the campaign spec when the supply stage runs; tests inject a
    /// scripted fake instead. On timed prewarm campaigns the same transport
    /// is kept alive past the supply stage for the dispatcher's
    /// pre-submission top-up ([`LadderTopup`]).
    pub supply_transport: Option<Arc<dyn SupplyTransport>>,
    /// Live narinfo source for the warm-set upstream-coverage probe.
    /// `None` when the archive's substituter lists carry no probeable
    /// (public-HTTPS) entry — only leaf-mode campaigns with a non-empty
    /// warm set need the probe, and they error at that point of use; every
    /// other campaign shape runs without it.
    pub narinfo: Option<Arc<dyn NarinfoSource>>,
    pub artifacts: Option<Arc<dyn ArtifactStore>>,
}

/// Entry point for `rio-replay run`: load and validate the spec,
/// materialize and open the replay archive, build the production backends,
/// and hand off to [`run_with_backends`].
///
/// # Operational contract
///
/// - **Pause:** touching `<state-dir>/PAUSE` pauses new submissions
///   (batches already running keep going); removing the file resumes. The
///   file is polled once per watchdog tick
///   (`knobs.cluster_status_poll_secs`, default 60s), so a pause or
///   unpause takes up to one tick to take effect. In timed scheduling mode
///   the file (and the engine's backpressure conditions) never gates
///   dispatch — warping the recorded cadence would destroy the property a
///   timed run measures — it is only recorded as a suspension window and
///   surfaces as dispatch lateness plus a timing-degraded flag; the
///   infra-failure threshold likewise becomes an abort recommendation
///   instead of a pause. The prefetch-shortfall pause of the supply stage
///   is unaffected (it acts before the execution clock starts in either
///   mode).
/// - **Exit code:** `0` both when the campaign drained completely and when
///   it stopped at the deadline with an explicitly-partial report —
///   consumers must read the `partial` flag in progress.json / summary.md,
///   not the exit code. Non-zero means an error (invalid spec or archive,
///   unreachable backends, state-dir I/O failure, or a dead background
///   task). A tripped regression gate does not change the exit code either:
///   the gate result is data (`report/gate.json`, mirrored in
///   progress.json) consumed by the operator CLI's `report --check`, never
///   by the pod exit code.
/// - **Deadline:** `--deadline` (or `spec.deadline`) stops *new*
///   submissions once reached; batches already in flight still drain,
///   which can take up to `knobs.batch_timeout_hours`. Do not set a
///   Kubernetes `activeDeadlineSeconds` close to the campaign deadline —
///   it would kill the pod mid-drain instead of letting the partial report
///   render. A supplied deadline that does not parse as RFC3339 is a
///   startup error.
/// - **Image requirements:** the per-tenant SSH key files and the service
///   HMAC key mounted at the paths named in the spec. The engine drives the
///   gateway's worker protocol in-process for builds, uploads, and the
///   prefetch arm — no `nix` binary or `ssh` client is needed — and the
///   archive input needs no extra tools either: the DwarFS image is opened
///   in place.
/// - **Resume:** re-running with the same state dir skips completed stages
///   and already-terminal jobs. Resuming on a *fresh* pod volume
///   additionally requires `spec.campaign_id` to be pinned (it names the
///   S3 prefix the synced state is restored from) and S3 to be configured.
/// - **S3 layout:** the replay archive is read from
///   `<archive.s3_bucket or s3.bucket>/<archive.s3_prefix>/…`; campaign
///   artifacts are synced to `<s3.bucket>/<s3.prefix>/<campaign-id>/…`
///   (default prefix `replay/campaigns`).
pub async fn run(args: RunArgs) -> Result<()> {
    let spec = CampaignSpec::load(&args.spec)?;
    let state = StateDir::new(&args.state_dir)?;
    // Campaign artifact store (periodic S3 sync of the state dir).
    let artifacts: Option<Arc<dyn ArtifactStore>> = match (&spec.s3.bucket, args.no_s3) {
        (Some(bucket), false) => Some(Arc::new(S3ArtifactStore::new(bucket.clone()).await)),
        _ => None,
    };
    // The archive may live in a different bucket than the campaign
    // artifacts; honor `archive.s3_bucket` when it names one.
    let archive_store: Option<Arc<dyn ArtifactStore>> = match &spec.archive.s3_bucket {
        Some(bucket) if !args.no_s3 && Some(bucket) != spec.s3.bucket.as_ref() => {
            Some(Arc::new(S3ArtifactStore::new(bucket.clone()).await))
        }
        _ => artifacts.clone(),
    };
    // The archive must be on local disk and open before the backends are
    // built (the client-ops submitter imports drv texts from it).
    let (_archive_path, archive) = ensure_archive(
        &state,
        args.archive.clone(),
        &spec,
        archive_store.as_deref(),
    )
    .await?;
    let admin = Arc::new(GrpcAdminApi::new(
        spec.cluster.scheduler_addr.clone(),
        spec.cluster.service_hmac_key_path.as_deref(),
    )?);
    // Gateway worker-protocol transport for build-path submissions. The
    // host-key pin is required by spec validation and re-required here: an
    // absent pin is an explicit error at this call site, never an empty
    // value passed downstream. Channel budget: a derived connection count
    // covers the scheduling mode's peak channel demand from the mode-wiring
    // table — the timed dispatcher holds up to max_sessions channels at
    // once, the timeless loop submit_concurrency — so admission can never
    // outrun transport capacity, and a burst of channel-open or exec
    // refusals against this configuration is a capacity or configuration
    // problem (the gateway shedding load at its global session cap, or the
    // connections knob is wrong), not per-unit infra noise.
    let endpoint = transport::GatewayEndpoint::parse(&spec.cluster.gateway_store_url)?;
    let policy = pinned_host_key_policy(&spec)?;
    let wiring = require_mode_wiring(&spec)?;
    let connections = spec
        .knobs
        .connections
        .unwrap_or_else(|| transport::default_connections(wiring.channel_demand));
    let pool = Arc::new(transport::GatewayPool::new(endpoint, policy, connections)?);
    tracing::info!(
        connections,
        channels_per_connection = transport::CHANNELS_PER_CONNECTION,
        channel_demand = wiring.channel_demand,
        "gateway transport configured"
    );
    // The supply stage's absent-upstream coverage probe is pointed at the
    // archive's declared substituters (target list first, then relay):
    // truth never comes from this client, only prefetch-path upstream
    // coverage does. The lists are archive-supplied input, so every entry
    // is classified once against the admission screen (public HTTPS =
    // probeable; s3 = supply-only; anything else unusable) and the probe
    // takes the FIRST probeable entry — a hostile or misbuilt entry is
    // skipped, never dialed, and a format-valid archive with no probeable
    // entry (e.g. an s3-only relay) still bootstraps: the probe is absent,
    // which only the leaf-mode warm-set probe actually requires.
    let classified =
        crate::nixcache::ClassifiedSubstituters::classify(&archive.manifest().substituters);
    for entry in classified.iter() {
        if let crate::nixcache::ArchiveSubstituterUrl::Unusable { url, reason } = entry {
            tracing::warn!(
                url = %url,
                reason = %reason,
                "skipping unusable archive substituter entry"
            );
        }
    }
    let narinfo: Option<Arc<dyn NarinfoSource>> = match classified.first_probeable() {
        Some(https) => Some(Arc::new(crate::nixcache::NixCacheClient::for_substituter(
            https,
            &crate::user_agent(None),
        )?)),
        None => {
            tracing::warn!(
                "archive lists no probeable public-HTTPS substituter; the warm-set \
                 upstream-coverage probe is unavailable for this campaign"
            );
            None
        }
    };
    let backends = Backends {
        store: Arc::new(GrpcStoreApi::new(
            spec.cluster.store_addr.clone(),
            STORE_QUERY_TIMEOUT,
        )),
        admin: admin.clone(),
        cluster: admin,
        submitter: Arc::new(ClientOpsSubmitter {
            pool,
            archive: Arc::new(drv_import::DrvArchive::new(archive.clone())),
            op_timeout: Duration::from_secs(spec.knobs.op_timeout_secs),
            probe_chunk: spec.knobs.probe_chunk,
        }),
        // Constructed from the spec when the supply stage actually runs (a
        // resumed campaign whose supply marker is already set never dials
        // the supply pools).
        supply_transport: None,
        narinfo,
        artifacts,
    };
    run_with_backends(args, spec, state, archive, backends).await
}

/// What one legal (scheduling mode × supply delivery) combination requires
/// of the engine. Produced by [`mode_wiring`] — the single source from which
/// spec validation derives legality, [`run`] sizes the gateway transport
/// pool, and [`run_with_backends`] wires the pre-submission supply hook, so
/// the three can never disagree about what a combination needs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ModeWiring {
    /// Peak concurrently held daemon channels the execute stage can demand
    /// from the gateway pool: the timed dispatcher admits up to
    /// `max_sessions` requests, each holding one build channel until it
    /// fully settles, while the timeless submit loop runs
    /// `submit_concurrency` workers, each holding one channel per batch.
    channel_demand: usize,
    /// The pre-submission supply top-up hook's role in this combination.
    topup: TopupRole,
}

/// Role of the pre-submission supply top-up hook ([`PreSubmitSupply`]) in a
/// (scheduling mode × supply delivery) combination.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TopupRole {
    /// Inline delivery: the supply stage deliberately defers every planned
    /// upload to the per-submission top-up, so the hook IS the delivery
    /// mechanism — wired whenever the stage produced a ladder context,
    /// regardless of the dependency policy (embedded input sources are
    /// deferred too).
    Primary,
    /// Prewarm delivery: planned supply is delivered before execution and
    /// the hook only backstops prewarm misses, so it is wired only when the
    /// dependency policy delivers anything per submission (skipped under
    /// `dependencies = "none"`).
    MissFallback,
}

/// The mode-wiring table: what each (scheduling mode × supply delivery)
/// combination requires of the engine, or `None` for a combination that has
/// no execution path. [`CampaignSpec::validate`] rejects exactly the `None`
/// combinations, [`run`] sizes the gateway pool from `channel_demand`, and
/// [`run_with_backends`] constructs the supply hook from `topup` — adding a
/// scheduling or delivery variant fails to compile until its arm (and with
/// it the legality, hook wiring, and capacity bound) is written here.
fn mode_wiring(mode: ScheduleMode, delivery: SupplyDelivery, knobs: &Knobs) -> Option<ModeWiring> {
    match (mode, delivery) {
        (ScheduleMode::Timeless, SupplyDelivery::Prewarm) => Some(ModeWiring {
            channel_demand: knobs.submit_concurrency,
            topup: TopupRole::MissFallback,
        }),
        (ScheduleMode::Timeless, SupplyDelivery::Inline) => Some(ModeWiring {
            channel_demand: knobs.submit_concurrency,
            topup: TopupRole::Primary,
        }),
        (ScheduleMode::Timed, SupplyDelivery::Prewarm) => Some(ModeWiring {
            // The dispatcher admits up to max_sessions concurrent requests
            // and each holds one build channel until it settles; the pool
            // must cover that bound, or admitted requests would block in
            // open_channel after their lateness was already stamped,
            // silently serializing the recorded cadence.
            channel_demand: knobs.submit_concurrency.max(knobs.max_sessions),
            topup: TopupRole::MissFallback,
        }),
        // Timed runs deliver all planned supply before the execution clock
        // starts: a per-submission inline top-up would spend schedule time
        // on uploads and corrupt the recorded cadence, so the combination
        // has no wiring and spec validation rejects it.
        (ScheduleMode::Timed, SupplyDelivery::Inline) => None,
    }
}

/// [`mode_wiring`] as a hard requirement: an absent wiring here means the
/// spec bypassed [`CampaignSpec::validate`], which rejects exactly the
/// combinations the table declines.
fn require_mode_wiring(spec: &CampaignSpec) -> Result<ModeWiring> {
    mode_wiring(spec.scheduling.mode, spec.supply.delivery, &spec.knobs).with_context(|| {
        format!(
            "scheduling.mode \"{}\" with supply.delivery \"{}\" has no engine wiring \
             (the campaign spec was not validated)",
            spec.scheduling.mode.as_str(),
            spec.supply.delivery.as_str(),
        )
    })
}

/// Resolve the SSH host-key policy every gateway pool the engine dials uses:
/// the spec's pinned `cluster.gateway_host_key` is required — an absent or
/// empty pin is an explicit error here, never an empty value passed
/// downstream (which would disable host-key verification).
fn pinned_host_key_policy(spec: &CampaignSpec) -> Result<transport::HostKeyPolicy> {
    let host_key = spec
        .cluster
        .gateway_host_key
        .clone()
        .filter(|key| !key.trim().is_empty())
        .context("cluster.gateway_host_key must be set for the client-ops transport")?;
    Ok(transport::HostKeyPolicy::Pinned(host_key))
}

/// Construct the production supply-stage transport from the campaign spec: a
/// build-tenant pool for validity probes and client uploads, plus a
/// prefetch-tenant pool (dialed with `<cluster.ssh_key_dir>/<tenants.warm_tenant>`)
/// when the effective supply policy includes the prefetch arm.
fn build_supply_transport(spec: &CampaignSpec) -> Result<Arc<dyn SupplyTransport>> {
    let endpoint = transport::GatewayEndpoint::parse(&spec.cluster.gateway_store_url)?;
    let policy = pinned_host_key_policy(spec)?;
    // The supply pools serve probe, upload, and prefetch traffic; deriving
    // the default connection count from those worker budgets (at the
    // transport's per-connection channel fan-out) keeps channel supply
    // ahead of the stage's demand.
    let connections = spec.knobs.connections.unwrap_or_else(|| {
        transport::default_connections(
            spec.knobs.probe_concurrency
                + spec.knobs.upload_workers
                + spec.knobs.submit_concurrency,
        )
    });
    let build = Arc::new(transport::GatewayPool::new(
        endpoint.clone(),
        policy.clone(),
        connections,
    )?);
    let prefetch =
        if spec.supply.effective_dependencies(spec.mode) == SupplyDependencies::Substituters {
            let key_dir = spec.cluster.ssh_key_dir.as_deref().context(
                "the supply prefetch arm requires cluster.ssh_key_dir (the per-tenant SSH key \
             directory) so it can dial the gateway as the prefetch tenant",
            )?;
            let key = supply::exec::tenant_key_path(key_dir, &spec.tenants.warm_tenant)?;
            let mut prefetch_endpoint = endpoint;
            prefetch_endpoint.ssh_key_path = key;
            Some(Arc::new(transport::GatewayPool::new(
                prefetch_endpoint,
                policy,
                connections,
            )?))
        } else {
            None
        };
    Ok(Arc::new(PoolSupplyTransport::new(
        build,
        prefetch,
        &spec.knobs,
    )))
}

/// SHA-256 (lowercase hex) and byte length of a local file, streamed in
/// 64 KiB chunks so multi-gigabyte archive images never load into memory.
fn file_sha256_and_size(path: &Path) -> Result<(String, u64)> {
    use std::io::Read as _;
    let mut file = std::fs::File::open(path).with_context(|| format!("open {}", path.display()))?;
    let mut hasher = sha2::Sha256::new();
    let mut size = 0u64;
    let mut buf = [0u8; 64 * 1024];
    loop {
        let n = file
            .read(&mut buf)
            .with_context(|| format!("read {}", path.display()))?;
        if n == 0 {
            break;
        }
        size += n as u64;
        hasher.update(&buf[..n]);
    }
    Ok((hex::encode(hasher.finalize()), size))
}

/// Download one object listed by the completion marker into `target`,
/// verifying its SHA-256 and size against the marker entry. An existing
/// local file that already matches is kept (resume on the same volume).
async fn fetch_archive_object(
    store: &dyn ArtifactStore,
    prefix: &str,
    object: &str,
    expected: &MemberDigest,
    target: &Path,
) -> Result<()> {
    if target.exists() {
        let (sha256, size) = file_sha256_and_size(target)?;
        if sha256 == expected.sha256 && size == expected.size {
            return Ok(());
        }
        tracing::warn!(
            object,
            "previously downloaded archive object does not match the completion marker; \
             re-downloading"
        );
    }
    let key = format!("{prefix}/{object}");
    let bytes = store
        .get_bytes(&key)
        .await?
        .ok_or_else(|| anyhow::anyhow!("archive object missing in S3: {key}"))?;
    let sha256 = hex::encode(sha2::Sha256::digest(&bytes));
    anyhow::ensure!(
        sha256 == expected.sha256 && bytes.len() as u64 == expected.size,
        "archive object {object} does not match its completion marker (marker sha256 {} size {}, \
         downloaded sha256 {sha256} size {})",
        expected.sha256,
        expected.size,
        bytes.len()
    );
    std::fs::write(target, bytes).with_context(|| format!("write {}", target.display()))?;
    Ok(())
}

/// Locate (or fetch) the replay archive and open it. Returns the local
/// archive path (image or directory form) and the open reader.
///
/// Without `--archive`, the archive is fetched from
/// `<archive.s3_prefix>/`: `complete.json` first (the upload marker —
/// without it the archive is incomplete), then every object it lists,
/// each verified against the marker's digests, and the standalone
/// `manifest.json` must hash to both the marker's archive id and the
/// spec's pinned digest before the image is opened.
pub async fn ensure_archive(
    state: &StateDir,
    explicit: Option<PathBuf>,
    spec: &CampaignSpec,
    artifacts: Option<&dyn ArtifactStore>,
) -> Result<(PathBuf, Arc<ReplayArchive>)> {
    let mut expected_id: Option<String> = None;
    let local = match explicit {
        Some(path) => path,
        None => {
            let Some(store) = artifacts else {
                bail!("--archive not given and no S3 configured to fetch the archive from");
            };
            let prefix = spec
                .archive
                .s3_prefix
                .clone()
                .ok_or_else(|| {
                    anyhow::anyhow!(
                        "spec.archive.s3_prefix is required when --archive is not given"
                    )
                })?
                .trim_matches('/')
                .to_string();
            let dest = state.path("archive");
            std::fs::create_dir_all(&dest)
                .with_context(|| format!("create archive dir {}", dest.display()))?;
            let marker_key = format!("{prefix}/{}", crate::archive::s3::ARCHIVE_COMPLETE_OBJECT);
            let marker_bytes = store.get_bytes(&marker_key).await?.ok_or_else(|| {
                anyhow::anyhow!(
                    "archive completion marker missing in S3: {marker_key} — the recorder uploads \
                     it last, so the archive is still uploading or the upload failed"
                )
            })?;
            let marker: CompleteMarker = serde_json::from_slice(&marker_bytes)
                .with_context(|| format!("parse {marker_key}"))?;
            for (object, digest) in &marker.objects {
                // The marker is remote input: only plain basenames may be
                // joined onto the local archive directory.
                anyhow::ensure!(
                    !object.is_empty()
                        && !object.contains('/')
                        && !object.contains('\\')
                        && object != "..",
                    "completion marker {marker_key} lists a non-basename object name {object:?}; \
                     refusing to write outside {}",
                    dest.display()
                );
                fetch_archive_object(store, &prefix, object, digest, &dest.join(object)).await?;
            }
            let manifest_path = dest.join(ARCHIVE_MANIFEST_OBJECT);
            let manifest_bytes = std::fs::read(&manifest_path)
                .with_context(|| format!("read {}", manifest_path.display()))?;
            let manifest_id = hex::encode(sha2::Sha256::digest(&manifest_bytes));
            anyhow::ensure!(
                manifest_id == marker.archive_id,
                "downloaded manifest.json hashes to {manifest_id} but the completion marker says \
                 the archive id is {}",
                marker.archive_id
            );
            anyhow::ensure!(
                manifest_id == spec.archive.digest,
                "archive manifest digest mismatch: spec pins {}, fetched {manifest_id}",
                spec.archive.digest
            );
            expected_id = Some(marker.archive_id.clone());
            dest.join(ARCHIVE_IMAGE_OBJECT)
        }
    };
    // Opening a DwarFS image is blocking work (directory-form archives are
    // cheap, but the call is uniform).
    let open_path = local.clone();
    let archive = tokio::task::spawn_blocking(move || ReplayArchive::open(&open_path))
        .await
        .context("archive open task panicked or was cancelled")?
        .with_context(|| format!("open replay archive {}", local.display()))?;
    if let Some(expected) = expected_id {
        // The manifest member inside the image is the identity; it must be
        // the same bytes the standalone manifest.json carried.
        anyhow::ensure!(
            archive.archive_id() == Some(expected.as_str()),
            "the downloaded archive image's manifest member hashes to {:?} but the standalone \
             manifest.json (and completion marker) say {expected}",
            archive.archive_id()
        );
    }
    Ok((local, Arc::new(archive)))
}

/// Plan-time excluded jobs carry their exclusion as `rio.outcome` with no
/// build/exec fields: write their terminal records right after planning.
/// The exclusion vocabulary equals the disposition names, so both fields
/// are written via [`Disposition::as_str`] (never hand-typed literals).
/// `divergent_in_scope` lists the in-scope jobs the recorder marked
/// identity-divergent: their evaluation here cannot be compared against the
/// recorded truth, so they are retired up front under the
/// identity-divergent disposition instead of being submitted.
fn write_plan_time_records(
    state: &StateDir,
    manifest: &[archive_input::ManifestEntry],
    plan: &PlanOutput,
    divergent_in_scope: &[String],
    mode: &str,
    existing: &BTreeMap<String, JobRecord>,
) -> Result<()> {
    let by_job: HashMap<&str, &archive_input::ManifestEntry> =
        manifest.iter().map(|m| (m.job.as_str(), m)).collect();
    let emit = |job: &str, disposition: Disposition| -> Result<()> {
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
                    outcome: disposition.as_str().to_string(),
                    ..Default::default()
                },
                expected: model::ExpectedSide {
                    outcome: model::ExpectedOutcome::Unknown.as_str().to_string(),
                    ..Default::default()
                },
                nar_compare: BTreeMap::new(),
                verdict: None,
                disposition: Some(disposition.as_str().to_string()),
                cascaded: false,
                failure_cause: None,
                flaky: false,
                signature: None,
                log_key: None,
                repro: String::new(),
                evidence: None,
                updated_at: now_rfc3339(),
            },
        )
    };
    for job in plan.skipped.keys() {
        emit(job, Disposition::Filtered)?;
    }
    for job in &plan.not_attemptable {
        emit(job, Disposition::NotAttemptable)?;
    }
    for job in &plan.cached_prior_jobs {
        emit(job, Disposition::CachedPrior)?;
    }
    for job in divergent_in_scope {
        emit(job, Disposition::IdentityDivergent)?;
    }
    Ok(())
}

/// Partial report (deadline/abort): every in-scope job that never reached a
/// record gets an explicit not-attempted [`JobRecord`] (rio outcome and
/// disposition "not-attempted", no build/exec fields), so the report's
/// per-class counts sum to the in-scope total. Returns how many were
/// written.
fn write_not_attempted_records(
    state: &StateDir,
    manifest: &[archive_input::ManifestEntry],
    plan: &PlanOutput,
    mode: &str,
    existing: &BTreeMap<String, JobRecord>,
) -> Result<usize> {
    let by_job: HashMap<&str, &archive_input::ManifestEntry> =
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
                expected: model::ExpectedSide {
                    outcome: model::ExpectedOutcome::Unknown.as_str().to_string(),
                    ..Default::default()
                },
                nar_compare: BTreeMap::new(),
                verdict: None,
                disposition: Some(Disposition::NotAttempted.as_str().to_string()),
                cascaded: false,
                failure_cause: None,
                flaky: false,
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

/// Jobs whose latest record carries a terminal class (any verdict, or any
/// disposition other than not-attempted).
fn terminal_set(records: &BTreeMap<String, JobRecord>) -> HashSet<String> {
    records
        .iter()
        .filter(|(_, r)| model::is_terminal_class(&r.verdict, &r.disposition))
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
/// the infra-indeterminate verdict with the "stalled-active" /
/// "stalled-queued" signature).
fn stall_record(
    ctx: &JobContext,
    signature: &str,
    mode: &str,
    campaign_id: &str,
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
        expected: model::ExpectedSide {
            outcome: ctx.expected_outcome.as_str().to_string(),
            outputs: ctx.expected_outputs.clone(),
        },
        nar_compare: BTreeMap::new(),
        verdict: Some(Verdict::InfraIndeterminate.as_str().to_string()),
        disposition: None,
        cascaded: false,
        failure_cause: collect::failure_cause_for(&rio_outcome),
        flaky: false,
        signature: Some(signature.to_string()),
        log_key: None,
        repro: submitter::repro_command(campaign_id, &ctx.drv_path),
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
    campaign_id: &str,
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
                        campaign_id,
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
                    campaign_id,
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
    campaign_id: &str,
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
    let record = stall_record(ctx, signature, mode, campaign_id, attempts);
    state.append_jsonl(StateFile::Results, &record)?;
    results.lock().await.insert(job.to_string(), record);
    watchdog.lock().await.remove_job(job);
    tracker.in_flight.lock().await.remove(job);
    Ok(())
}

/// Inputs the timed scheduling arm needs, prepared once from the open
/// archive at the wiring point: the built schedule, the per-`(session, drv)`
/// timing lookup, the drv → job mapping for batch bookkeeping, and the
/// dispatcher tuning.
struct TimedInputs {
    schedule: Vec<ScheduledRequest>,
    timing: SharedTimingLookup,
    job_of_drv: Arc<BTreeMap<String, String>>,
    config: TimelineConfig,
}

/// What the poller does with this tick's backpressure conditions, by
/// scheduling mode: `(set the backpressure pause, recommend an abort)`.
///
/// Timeless campaigns pause new submissions on any condition (dispatch gap,
/// queue depth, infra-failure rate), exactly as before. Timed campaigns
/// never gate dispatch on them — pausing a timed run would destroy the
/// cadence it exists to measure — so the conditions stay advisory: the
/// infra-failure rate becomes an abort recommendation for the operator and
/// the rest only feed the watchdog's suspension windows.
fn pause_decision(
    mode: ScheduleMode,
    dispatch_pause: bool,
    queue_depth_pause: bool,
    infra_pause: bool,
) -> (bool, bool) {
    match mode {
        ScheduleMode::Timeless => (dispatch_pause || queue_depth_pause || infra_pause, false),
        ScheduleMode::Timed => (false, infra_pause),
    }
}

/// Convert one archive request record into the engine-owned schedule input,
/// sanitizing the recorded offset: non-finite values collapse to zero (the
/// request still replays, just without meaningful timing) and finite values
/// clamp into `[0, MAX_RECORDED_OFFSET_S]` so schedule construction can
/// never panic on a corrupt recording.
fn recorded_request_from(record: &RequestRecord) -> RecordedRequest {
    let offset_s = if record.offset_s.is_finite() {
        record.offset_s.clamp(0.0, MAX_RECORDED_OFFSET_S)
    } else {
        tracing::warn!(
            session = record.session,
            offset_s = record.offset_s,
            "recorded request offset is not finite; treating it as zero"
        );
        0.0
    };
    RecordedRequest {
        session: record.session,
        offset_s,
        targets: record
            .targets
            .iter()
            .map(|target| RecordedTarget {
                drv: target.drv.clone(),
                outputs: target.outputs.clone(),
            })
            .collect(),
    }
}

/// Convert one archive expected-outcome record into the per-unit timing
/// truth the timed schedule consumes. Non-finite (or, for durations,
/// negative) recorded values are dropped rather than poisoning the deadline
/// and disconnect math; oversized stop offsets are dropped too so the
/// disconnect timer falls back to its default delay instead of effectively
/// never firing.
fn recorded_timing_from(record: &OutcomeRecord) -> RecordedTiming {
    RecordedTiming {
        duration_s: record
            .duration_s
            .filter(|duration| duration.is_finite() && *duration >= 0.0),
        stop_offset_s: record
            .stop_offset_s
            .filter(|stop| stop.is_finite() && *stop <= MAX_RECORDED_OFFSET_S),
        interrupted: matches!(
            record.outcome,
            ExpectedOutcome::Cancelled | ExpectedOutcome::Disconnected
        ),
        expected_built: record.outcome == ExpectedOutcome::Built,
    }
}

/// The orchestrator: plan → truth load → supply → (execute ∥
/// collect ∥ watchdog ∥ sync) → report, with stage done-markers and resume.
/// The execute stage is the timeless submit loop or the timed dispatcher,
/// chosen by `spec.scheduling.mode`.
pub async fn run_with_backends(
    args: RunArgs,
    spec: CampaignSpec,
    state: StateDir,
    archive: Arc<ReplayArchive>,
    backends: Backends,
) -> Result<()> {
    let mut spec = spec;
    if let Some(limit) = args.limit {
        spec.filters.limit = Some(limit);
    }

    // ── Scheduling-mode capability gating ───────────────────────────────────
    // Checked before any stage marker or campaign record is written: a timed
    // campaign needs the archive's per-request offsets, and interruption
    // replay needs at least one recorded interruption to reproduce. The
    // effective replay flag (knob, possibly degraded here) drives schedule
    // construction below; the degradation is recorded as a low-confidence
    // comparability flag when the campaign record is first written.
    let mut replay_interruptions = spec.knobs.replay_interruptions;
    let mut scheduling_low_confidence: Vec<String> = Vec::new();
    if spec.scheduling.mode == ScheduleMode::Timed {
        anyhow::ensure!(
            archive.capabilities().timed,
            "scheduling.mode is \"timed\" but the archive does not declare the timed capability \
             (per-request offsets); record a timed archive or run the campaign timeless"
        );
        if replay_interruptions
            && !archive.outcomes().values().any(|record| {
                matches!(
                    record.outcome,
                    ExpectedOutcome::Cancelled | ExpectedOutcome::Disconnected
                )
            })
        {
            replay_interruptions = false;
            scheduling_low_confidence.push(report::FLAG_REPLAY_INTERRUPTIONS_DISABLED.to_string());
            tracing::warn!(
                "the archive records no cancellations or client disconnects; interruption \
                 replay is disabled for this campaign and the report is flagged low-confidence"
            );
        }
    }
    // The execute-stage wiring this (scheduling × delivery) combination
    // requires. Spec validation reads the same table for legality, so an
    // absent wiring can only mean the spec bypassed validation — refused
    // here, before any stage marker or campaign record is written.
    let wiring = require_mode_wiring(&spec)?;

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
    let archive_id = archive
        .archive_id()
        .context("the campaign engine requires a v1 archive (this archive has no archive id)")?
        .to_string();
    let mut campaign = match state.read_json::<CampaignRecord>("campaign.json")? {
        Some(existing) => {
            // Resume gate: one campaign must never mix two archives.
            anyhow::ensure!(
                existing.archive.archive_id == archive_id,
                "campaign.json pins archive {} but the provided archive is {archive_id}",
                existing.archive.archive_id
            );
            tracing::info!(campaign_id = %existing.campaign_id, "resuming an existing campaign");
            existing
        }
        None => {
            let result = plan::run_plan(
                &spec,
                &archive,
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
            // Scheduling-mode degradations decided at bootstrap (currently
            // only forced-off interruption replay) join the plan-time flags
            // on the first campaign.json write.
            record
                .comparability
                .low_confidence
                .extend(scheduling_low_confidence.iter().cloned());
            // Recorder-side exclusions (eval errors, aggregates) never become
            // workload units, so they enter the comparability accounting here
            // and are merged — never overwritten — by the report-time refresh.
            record.comparability.excluded = archive_input::exclusion_counts(&archive);
            record.comparability.exclusions_recorded = archive_input::exclusions_recorded(&archive);
            // Archive provenance: when the archive was recorded and how old
            // it was when this campaign started, both part of what makes two
            // reports comparable.
            let campaign_created_at = record.created_at.clone();
            record
                .comparability
                .record_archive_provenance(archive.manifest().created_at, &campaign_created_at);
            record.plan = Some(result.output.clone());
            state.write_json_atomic("campaign.json", &record)?;
            record
        }
    };
    let plan_output = campaign
        .plan
        .clone()
        .context("campaign.json has no plan output")?;
    let manifest = archive_input::load_units(&archive)?;
    let dep_closure = archive_input::load_closures(&archive, &manifest)?;
    let in_scope: HashSet<&str> = plan_output.in_scope.iter().map(String::as_str).collect();
    // Identity-divergent units that made it into scope are retired at plan
    // time (eval-divergence) and never offered to the submit loop.
    let divergent_in_scope: Vec<String> = archive_input::identity_divergent_units(&archive)?
        .into_iter()
        .filter(|job| in_scope.contains(job.as_str()))
        .collect();
    let existing_records = latest_per_job(state.load_jsonl(StateFile::Results)?);
    if !state.marker_done("plan") {
        write_plan_time_records(
            &state,
            &manifest,
            &plan_output,
            &divergent_in_scope,
            spec.mode.as_str(),
            &existing_records,
        )?;
        state.set_marker("plan")?;
    }

    // The dependency-output producer map is not persisted in campaign.json —
    // recompute it.
    let warm_comp = plan::compute_warm_sets(&manifest, &dep_closure, &plan_output.in_scope);

    // ── Stage: truth load ───────────────────────────────────────────────────
    // Per-unit expected outcomes come from the archive's recorded truth (no
    // outbound queries), so the load is pure, cheap, and re-derived on every
    // start — it needs no done-marker.
    let truth = truth::expected_outcomes_for_units(&archive, &manifest)?;

    // ── Stage: supply ───────────────────────────────────────────────────────
    let batch_seq = Arc::new(AtomicU64::new(
        state
            .load_jsonl::<model::BatchRecord>(StateFile::Batches)?
            .iter()
            .map(|b| b.batch_id + 1)
            .max()
            .unwrap_or(1),
    ));
    // Pre-submission supply hook for the execute stage (the timed
    // dispatcher's per-request call, the timeless submit loop's per-batch
    // call): built below only when the supply stage runs in this process
    // under a wiring that has something to top up; `None` keeps the execute
    // stage running without one.
    let mut supply_topup: Option<Arc<dyn PreSubmitSupply>> = None;
    if !state.marker_done("supply") {
        let effective_dependencies = spec.supply.effective_dependencies(spec.mode);
        // Outputs (and drvs) of the units that remain attemptable: those
        // outputs are what the campaign measures, so they are never
        // supplied; everything else in their closures is fair game for the
        // supply ladder.
        let attempt_excluded: HashSet<&str> = plan_output
            .not_attemptable
            .iter()
            .chain(plan_output.cached_prior_jobs.iter())
            .chain(divergent_in_scope.iter())
            .map(String::as_str)
            .collect();
        let mut workload_drvs = BTreeSet::new();
        let mut workload_outputs = BTreeSet::new();
        for m in manifest.iter().filter(|m| {
            in_scope.contains(m.job.as_str()) && !attempt_excluded.contains(m.job.as_str())
        }) {
            workload_drvs.insert(m.drv_path.clone());
            workload_outputs.extend(m.outputs.values().cloned());
        }
        // The prefetch arm only exists under the substituters policy; the
        // other policies deliver dependencies by client upload (or withhold
        // them entirely).
        let prefetch_paths: BTreeMap<String, Option<String>> =
            if effective_dependencies == SupplyDependencies::Substituters {
                plan_output
                    .warm_set
                    .iter()
                    .map(|path| (path.clone(), warm_comp.producer.get(path).cloned()))
                    .collect()
            } else {
                BTreeMap::new()
            };
        // Upstream coverage for the prefetch set: probe the archive's
        // declared substituter for every warm path so the supply stage never
        // asks the target to substitute a path its substituter cannot serve.
        // Absent paths are pre-classified `unavailable` in supply.jsonl by
        // the probe; found paths seed the ladder's coverage set. Running
        // under the supply gate means a resumed campaign that has not
        // finished its supply stage re-probes — resume costs re-probing,
        // never correctness.
        let target_coverage: BTreeSet<String> =
            if spec.mode == Mode::Leaf && !plan_output.warm_set.is_empty() {
                // The probe's single point of use: only here does a missing
                // probeable substituter become an error, so campaigns that
                // never probe (non-leaf, empty warm set, resumed past
                // supply) are not held to a requirement they don't have.
                let narinfo = backends.narinfo.as_ref().context(
                    "the warm-set upstream-coverage probe needs a public-HTTPS substituter, but \
                     the archive's substituter lists (target, then relay) contain no usable \
                     entry; a leaf-mode campaign with a non-empty warm set cannot run against \
                     this archive",
                )?;
                truth::probe_warm_upstream_coverage(
                    &state,
                    narinfo.as_ref(),
                    &plan_output.warm_set,
                    spec.knobs.narinfo_concurrency,
                    NARINFO_SWEEP_ATTEMPTS,
                )
                .await?
                .found
            } else {
                BTreeSet::new()
            };
        let transport = match &backends.supply_transport {
            Some(transport) => transport.clone(),
            None => build_supply_transport(&spec)?,
        };
        let inputs = SupplyInputs {
            workload_outputs,
            workload_drvs,
            prefetch_paths,
            prior_valid: plan_output.cached_prior_paths.iter().cloned().collect(),
            target_coverage,
            archive: Some(archive.clone()),
            target_substituters: spec.supply.target_substituters.clone(),
            relay_substituters: archive.manifest().substituters.relay.clone(),
            dependencies: effective_dependencies,
            delivery: spec.supply.delivery,
        };
        // Production blocks on the prefetch-shortfall pause (the operator
        // resumes by removing the PAUSE file); the report is persisted so
        // the final report and resumed runs can re-read it.
        let supply_output = run_supply_stage(
            state.clone(),
            transport.clone(),
            inputs,
            &spec.knobs,
            batch_seq.clone(),
            true,
        )
        .await?;
        state.write_json_atomic("supply-report.json", &supply_output.report)?;
        state.set_marker("supply")?;
        // The mode-wiring table decides whether the stage's transport and
        // ladder context outlive the stage as the pre-submission top-up
        // hook. Under inline delivery the hook IS the delivery mechanism:
        // the stage deferred every planned upload to it, whatever the
        // dependency policy. Under prewarm it only backstops prewarm misses
        // (a path the prewarm pass refused, failed, or skipped gets one
        // more delivery attempt), so dependencies-none campaigns — whose
        // ladder delivers nothing per submission — run without it. Either
        // way the hook reuses the stage's admitted substituters and probe
        // results instead of re-admitting or re-probing what is already
        // known valid.
        //
        // TODO: rebuild the ladder context when resuming an inline-delivery
        // campaign. A resumed campaign whose supply marker is already set
        // ran the stage in an earlier process, so no context exists here
        // and the execute stage runs without the hook: under prewarm that
        // only loses the miss-fallback, but under inline the deferred
        // uploads of jobs the earlier process never submitted are not
        // delivered, and those jobs fail on missing inputs. Re-running the
        // stage's planning and probing on resume (resume costs re-probing,
        // never correctness) would restore delivery.
        let wire_topup = match wiring.topup {
            TopupRole::Primary => true,
            TopupRole::MissFallback => effective_dependencies != SupplyDependencies::None,
        };
        if wire_topup && let Some(ladder) = supply_output.ladder {
            let topup: Arc<dyn PreSubmitSupply> = Arc::new(LadderTopup::new(
                transport,
                archive.clone(),
                ladder,
                spec.knobs.clone(),
                state.clone(),
            ));
            supply_topup = Some(topup);
        }
    }
    // Supply summary for progress.json while the campaign executes (the
    // final report re-reads it after the journal stops growing). On resume
    // the persisted report from the run that completed the stage is used.
    let supply_summary = load_supply_summary(&state)?;

    // ── Main loop: submit ∥ collect ∥ watchdog ∥ sync ───────────────────────
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
    let divergent: HashSet<&str> = divergent_in_scope.iter().map(String::as_str).collect();
    let mut contexts: HashMap<String, JobContext> = HashMap::new();
    let mut attemptable: Vec<batch::PendingJob> = Vec::new();
    for m in manifest
        .iter()
        .filter(|m| in_scope.contains(m.job.as_str()))
    {
        // Expected truth comes from the archive; a unit the loader produced
        // no entry for (cannot happen for units it was given) degrades to
        // unknown rather than panicking.
        let unit_truth = truth
            .get(&m.job)
            .cloned()
            .unwrap_or_else(|| truth::UnitTruth {
                outcome: model::ExpectedOutcome::Unknown,
                side: model::ExpectedSide {
                    outcome: model::ExpectedOutcome::Unknown.as_str().to_string(),
                    ..Default::default()
                },
            });
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
                expected_outcome: unit_truth.outcome,
                expected_outputs: unit_truth.side.outputs.clone(),
                plan_not_attemptable: not_attemptable.contains(m.job.as_str()),
                plan_snapshot_valid: cached_prior_jobs.contains(m.job.as_str()),
            },
        );
        if !not_attemptable.contains(m.job.as_str())
            && !cached_prior_jobs.contains(m.job.as_str())
            && !divergent.contains(m.job.as_str())
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
    // Timed-mode pause semantics: the poller never gates dispatch on
    // backpressure, it only latches an abort recommendation (infra-failure
    // rate) and a timing-degraded flag (a pause/dispatch suspension window
    // closed while the schedule was executing). Both are written by the
    // poller and read when the timed run statistics are assembled; they stay
    // false for timeless campaigns.
    let abort_recommended = Arc::new(AtomicBool::new(false));
    let timing_degraded = Arc::new(AtomicBool::new(false));
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
        let schedule_mode = spec.scheduling.mode;
        let abort_recommended = abort_recommended.clone();
        let timing_degraded = timing_degraded.clone();
        let supply_for_progress = supply_summary.clone();
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
                            .filter(|r| {
                                r.verdict.as_deref() == Some(Verdict::InfraIndeterminate.as_str())
                            })
                            .count();
                        Some((infra as f64 / window.len() as f64) * 100.0)
                    } else {
                        None
                    };
                    let terminal_in_scope = res
                        .iter()
                        .filter(|(job, r)| {
                            in_scope_jobs.contains(job.as_str())
                                && model::is_terminal_class(&r.verdict, &r.disposition)
                        })
                        .count();
                    (terminal_in_scope, infra_rate_pct)
                };
                let infra_pause = infra_rate_pct.is_some_and(|rate| rate > knobs.infra_pause_pct);
                let (backpressure, recommend_abort) = pause_decision(
                    schedule_mode,
                    outcome.dispatch_pause,
                    queue_depth_pause,
                    infra_pause,
                );
                pause.set_backpressure(backpressure);
                if recommend_abort && !abort_recommended.swap(true, Ordering::SeqCst) {
                    tracing::error!(
                        infra_rate_pct,
                        threshold_pct = knobs.infra_pause_pct,
                        "infra-failure rate exceeds the pause threshold; pausing would distort \
                         the recorded cadence, so the campaign keeps dispatching — aborting it \
                         is recommended if the failures persist"
                    );
                }
                // A pause or dispatch-gap suspension window that closed while
                // the timed schedule was executing means recorded cadence was
                // not honored for its duration: the timing-fidelity numbers
                // are degraded.
                if schedule_mode == ScheduleMode::Timed
                    && let Some(window) = &outcome.closed_window
                    && window
                        .components
                        .iter()
                        .any(|c| c == COMPONENT_PAUSE || c == COMPONENT_DISPATCH)
                {
                    timing_degraded.store(true, Ordering::SeqCst);
                }
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
                        abort_recommended = abort_recommended.load(Ordering::SeqCst),
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
                        &campaign_id,
                    )
                    .await
                {
                    tracing::warn!(
                        error = %format!("{e:#}"),
                        "applying stall verdicts failed; retrying on the next poll"
                    );
                }
                // progress.json (atomic rewrite — the status loop polls it)
                // + periodic S3 sync. The timed summary only exists once the
                // dispatcher has drained, so mid-run progress carries none.
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
                        supply_for_progress.as_ref(),
                        None,
                        abort_recommended.load(Ordering::SeqCst),
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
        let campaign_id = campaign_id.clone();
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
                        &campaign_id,
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

    // Timed scheduling inputs: the recorded requests, their timing truth,
    // the drv → job mapping, and the dispatcher tuning, prepared once from
    // the archive. Timeless campaigns need none of this. Recorded values are
    // sanitized at this conversion point so corrupt offsets can never reach
    // the schedule math.
    let timed_inputs = match spec.scheduling.mode {
        ScheduleMode::Timeless => None,
        ScheduleMode::Timed => {
            let requests: Vec<RecordedRequest> = archive
                .requests()
                .iter()
                .map(recorded_request_from)
                .collect();
            let timing: SharedTimingLookup = {
                let archive = archive.clone();
                Arc::new(move |session, drv| {
                    archive
                        .expected_outcome(session, drv)
                        .map(recorded_timing_from)
                })
            };
            let schedule = build_schedule(
                &requests,
                &|session, drv| timing(session, drv),
                spec.knobs.speedup,
                spec.filters.limit,
                replay_interruptions,
            );
            let job_of_drv: BTreeMap<String, String> = manifest
                .iter()
                .map(|m| (m.drv_path.clone(), m.job.clone()))
                .collect();
            let mut config = TimelineConfig::from_knobs(&spec.knobs);
            config.replay_interruptions = replay_interruptions;
            tracing::info!(
                requests = requests.len(),
                scheduled = schedule.len(),
                speedup = spec.knobs.speedup,
                replay_interruptions,
                max_sessions = config.max_sessions,
                "timed scheduling mode: replaying the recorded request schedule"
            );
            Some(TimedInputs {
                schedule,
                timing,
                job_of_drv: Arc::new(job_of_drv),
                config,
            })
        }
    };

    // Outer drain loop. Timeless mode submits until drained, runs a final
    // synchronous collect pass to catch the tail, and repeats while that
    // pass re-queued work and the deadline has not fired. Timed mode runs
    // the dispatcher exactly once (the timeline drains itself and owns its
    // own retries) and shares the final collect pass and the
    // deadline-partial backfill. The body is wrapped so the stop signal and
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
            match &timed_inputs {
                None => {
                    // The supply stage's top-up hook (when one was built
                    // above) gives every batch a pre-submission gap top-up:
                    // the delivery mechanism itself under inline delivery,
                    // the miss-fallback under prewarm.
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
                        supply_topup.clone(),
                    )
                    .await?;
                }
                Some(inputs) => {
                    // Requests whose every target is already terminal (a
                    // resumed run) are skipped from this snapshot; the
                    // dispatcher never consults the pause state. The supply
                    // stage's top-up hook (when one was built above) gives
                    // every request a pre-submission gap top-up — the inline
                    // fallback for prewarm misses.
                    let terminal = terminal_set(&*results.lock().await);
                    let mut stats = run_timed_dispatch(
                        state.clone(),
                        backends.submitter.clone(),
                        tracker.clone(),
                        inputs.schedule.clone(),
                        inputs.job_of_drv.clone(),
                        inputs.timing.clone(),
                        spec.cluster.gateway_store_url.clone(),
                        inputs.config.clone(),
                        spec.knobs.clone(),
                        batch_seq.clone(),
                        deadline_reached,
                        supply_topup.clone(),
                        Arc::new(tokio::sync::Mutex::new(terminal)),
                    )
                    .await?;
                    // A pause/dispatch suspension window during execution
                    // degrades timing fidelity exactly like a resume does;
                    // the persisted statistics feed the report.
                    stats.timing_degraded =
                        stats.timing_degraded || timing_degraded.load(Ordering::SeqCst);
                    state.write_json_atomic("timed-stats.json", &stats)?;
                }
            }
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
                    &campaign_id,
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
            // The timed schedule drains in a single pass (the dispatcher
            // owns its retries); the timeless loop repeats while the collect
            // pass re-queued work.
            if timed_inputs.is_some() || requeued == 0 {
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
    // Re-read the supply and timed summaries for the final report: the
    // per-batch top-up can append supply.jsonl entries during execution and
    // the timed dispatcher persists its statistics only once it drains.
    let supply_summary = load_supply_summary(&state)?;
    let timed_summary: Option<TimedRunStats> = state.read_json("timed-stats.json")?;
    let final_abort_recommended = abort_recommended.load(Ordering::SeqCst);
    // Refresh the comparability block in campaign.json with final counts,
    // the supply/timing context (prefetch shortfall, timed degradation), and
    // the re-derived low-confidence flags.
    let agg = report::aggregate(&final_records);
    let empty_counts = BTreeMap::new();
    let plan_counts = campaign
        .plan
        .as_ref()
        .map(|p| &p.counts)
        .unwrap_or(&empty_counts);
    campaign.comparability = report::comparability_with_counts(
        &campaign.comparability,
        &agg,
        plan_counts,
        &spec.knobs,
        supply_summary.as_ref(),
        timed_summary.as_ref(),
    );
    state.write_json_atomic("campaign.json", &campaign)?;
    let plan_count_u64 = |key: &str| {
        plan_counts
            .get(key)
            .map(|v| u64::try_from(*v).unwrap_or(u64::MAX))
    };
    let input = report::ReportInput {
        campaign: &campaign,
        records: &final_records,
        suspension: &suspension,
        generated_at: now_rfc3339(),
        partial,
        top_n: spec.knobs.report_top_n,
        supply: supply_summary.as_ref(),
        timed: timed_summary.as_ref(),
        abort_recommended: final_abort_recommended,
        plan_rss_mib: plan_count_u64(PLAN_COUNT_RSS_BEFORE),
        plan_rss_peak_mib: plan_count_u64(PLAN_COUNT_RSS_PEAK),
    };
    report::write_report(&state, &input)?;
    let progress = report::build_progress(
        &campaign,
        &final_records,
        &suspension,
        "done",
        now_rfc3339(),
        None,
        supply_summary.as_ref(),
        timed_summary.as_ref(),
        final_abort_recommended,
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

/// Read the persisted supply-stage report back and re-derive its per-path
/// outcome counts from the supply journal (latest record per path wins, so
/// a path recorded by both the coverage probe and the supply stage — or
/// re-supplied by a later arm — is counted once, under its settled
/// disposition). `None` when the campaign state has no supply report.
fn load_supply_summary(state: &StateDir) -> Result<Option<SupplyStageReport>> {
    let Some(mut report) = state.read_json::<SupplyStageReport>("supply-report.json")? else {
        return Ok(None);
    };
    let entries: Vec<SupplyEntry> = state.load_jsonl(StateFile::Supply)?;
    refresh_outcome_counts(&mut report, &entries);
    Ok(Some(report))
}

/// Collect-loop backend bundle (subset of [`Backends`], clonable into the
/// background collect task).
struct CollectBackends {
    admin: Arc<dyn AdminApi>,
    store: Arc<dyn StoreApi>,
    artifacts: Option<Arc<dyn ArtifactStore>>,
}

/// One collect pass over every settled, not-yet-processed build batch
/// (submit-loop and timed-dispatcher kinds alike): classify each batch's
/// jobs via [`process_settled_batch`], count an engine resubmission for
/// every re-queued job, and persist the processed-batch set
/// (collected.json) so resume never re-processes a batch. Returns how many
/// job re-queues the pass produced.
#[allow(clippy::too_many_arguments)]
async fn collect_pass_with(
    state: &StateDir,
    backends: &CollectBackends,
    contexts: &HashMap<String, JobContext>,
    tracker: &SubmitTracker,
    processed: &mut HashSet<u64>,
    knobs: &Knobs,
    mode: &str,
    campaign_id: &str,
    artifact_prefix: Option<&str>,
) -> Result<usize> {
    let batches: Vec<model::BatchRecord> = state.load_jsonl(StateFile::Batches)?;
    let mut requeued = 0usize;
    for batch in batches {
        // Both build-batch kinds are collected here: the timeless submit
        // loop's batches and the timed dispatcher's. Anything else (e.g. a
        // kind written by an older engine version) is skipped, never failed.
        let collectable = matches!(batch.kind.as_str(), BATCH_KIND_SUBMIT | BATCH_KIND_TIMED);
        if !collectable || processed.contains(&batch.batch_id) {
            continue;
        }
        let view = BatchView {
            kind: batch.kind.clone(),
            build_id: batch.build_id.clone(),
            results: batch.results.clone(),
            reasons: batch.reasons.clone(),
            stderr_tail: batch.stderr_tail.clone(),
            engine_cancelled: batch.engine_cancelled,
            interruption_drvs: batch.interruption_drvs.clone(),
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
            campaign_id,
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

    use crate::run::archive_input::{
        load_closures, load_units, write_mini_archive, write_mini_timed_archive,
    };
    use crate::run::grpc::test_support::FakeStoreApi;
    use crate::run::grpc::{ClusterCounts, GraphSnapshot, IceSnapshot, PoisonedView};
    use crate::run::model::{
        DispatchEntry, ExpectedOutcome, PathOutcome, SUPPLY_OUTCOME_DELEGATED,
        SUPPLY_OUTCOME_DELIVERED, SUPPLY_OUTCOME_REFUSED, SUPPLY_OUTCOME_UNAVAILABLE, SupplyEntry,
        build_status_name,
    };
    use crate::run::submitter::BatchOutcome;
    use crate::run::submitter::test_support::FakeSubmitter;
    use crate::run::supply::exec::test_support::FakeSupplyTransport;
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

    /// Scripted submitter keyed by each batch's first root drv. The timed
    /// dispatcher submits concurrently dispatched requests from independent
    /// tasks, so an order-scripted [`FakeSubmitter`] could pop the wrong
    /// outcome under scheduler jitter; keying by root removes the order
    /// dependence. Unscripted roots settle with a default (empty) outcome.
    #[derive(Default)]
    struct KeyedSubmitter {
        outcomes: std::sync::Mutex<HashMap<String, BatchOutcome>>,
        submitted: std::sync::Mutex<Vec<batch::Batch>>,
    }

    #[async_trait]
    impl Submitter for KeyedSubmitter {
        async fn submit_batch(
            &self,
            _store_url: &str,
            batch: &batch::Batch,
            _timeout: Duration,
        ) -> Result<BatchOutcome> {
            self.submitted.lock().unwrap().push(batch.clone());
            Ok(self
                .outcomes
                .lock()
                .unwrap()
                .get(&batch.root_drvs[0])
                .cloned()
                .unwrap_or_default())
        }
    }

    fn narinfo_text(path: &str) -> String {
        format!(
            "StorePath: {path}\nURL: nar/x.nar.zst\nCompression: zstd\nNarHash: sha256:{}\nNarSize: 10\nReferences: \n",
            "0".repeat(52)
        )
    }

    fn leaf_spec(archive_digest: &str) -> CampaignSpec {
        let mut spec: CampaignSpec = serde_json::from_str(&format!(
            r#"{{
              "campaign_id": "c-e2e",
              "mode": "leaf",
              "archive": {{"digest": "{archive_digest}"}},
              "cluster": {{"gateway_store_url": "ssh-ng://rio@gw:22?ssh-key=/k",
                          "ssh_key_dir": "/keys",
                          "scheduler_addr": "s:9001", "store_addr": "st:9002"}},
              "tenants": {{"build_tenant": "replay-leaf", "warm_tenant": "replay-warm",
                          "upstreams_verified": true}},
              "filters": {{"systems": ["x86_64-linux"], "exclude_features": ["kvm"]}},
              "s3": {{"prefix": "replay/campaigns"}}
            }}"#
        ))
        .unwrap();
        // Tight loop intervals so the end-to-end test finishes fast.
        spec.knobs.collect_poll_secs = 1;
        spec.knobs.cluster_status_poll_secs = 1;
        spec.knobs.s3_sync_interval_secs = 1;
        spec
    }

    fn run_args(state_dir: &Path) -> RunArgs {
        RunArgs {
            spec: PathBuf::from("/dev/null"),
            state_dir: state_dir.to_path_buf(),
            archive: None,
            limit: None,
            deadline: None,
            allow_unverified_tenants: false,
            no_s3: true,
        }
    }

    /// Write the mini replay archive into a tempdir and open it, returning
    /// the directory guard, the open reader handle, and the archive id.
    fn open_mini_archive() -> (tempfile::TempDir, Arc<ReplayArchive>, String) {
        let dir = tempfile::tempdir().unwrap();
        let built = write_mini_archive(dir.path());
        let archive = Arc::new(ReplayArchive::open(dir.path()).unwrap());
        (dir, archive, built.archive_id)
    }

    /// Minimal terminal (match-built) record for tests that only need a
    /// terminal class, not full evidence.
    fn terminal_record(job: &str) -> JobRecord {
        JobRecord {
            job: job.into(),
            system: "x86_64-linux".into(),
            drv_path: format!("/nix/store/{}-x.drv", "a".repeat(32)),
            mode: "leaf".into(),
            attempts: 1,
            build_ids: vec![],
            rio: model::RioSide::default(),
            expected: model::ExpectedSide::default(),
            nar_compare: BTreeMap::new(),
            verdict: Some(Verdict::MatchBuilt.as_str().into()),
            disposition: None,
            cascaded: false,
            failure_cause: None,
            flaky: false,
            signature: None,
            log_key: None,
            repro: String::new(),
            evidence: None,
            updated_at: now_rfc3339(),
        }
    }

    /// The mode-wiring table is the single source for mode legality, hook
    /// role, and transport demand: every legal (scheduling × delivery)
    /// combination produces a wiring, the one unsupported combination
    /// (timed × inline) produces none, and the timed channel demand covers
    /// the dispatcher's admission bound so a derived pool can never admit
    /// more concurrent requests than it can carry.
    #[test]
    fn mode_wiring_is_the_single_source_for_legality_hooks_and_demand() {
        // Defaults: submit_concurrency 8, max_sessions 32.
        let knobs = Knobs::default();
        let timeless_prewarm =
            mode_wiring(ScheduleMode::Timeless, SupplyDelivery::Prewarm, &knobs).unwrap();
        assert_eq!(timeless_prewarm.channel_demand, 8);
        assert_eq!(timeless_prewarm.topup, TopupRole::MissFallback);
        let timeless_inline =
            mode_wiring(ScheduleMode::Timeless, SupplyDelivery::Inline, &knobs).unwrap();
        assert_eq!(timeless_inline.channel_demand, 8);
        assert_eq!(timeless_inline.topup, TopupRole::Primary);
        let timed_prewarm =
            mode_wiring(ScheduleMode::Timed, SupplyDelivery::Prewarm, &knobs).unwrap();
        assert_eq!(
            timed_prewarm.channel_demand, 32,
            "the timed demand must cover max_sessions, not just submit_concurrency"
        );
        assert_eq!(timed_prewarm.topup, TopupRole::MissFallback);
        assert!(
            mode_wiring(ScheduleMode::Timed, SupplyDelivery::Inline, &knobs).is_none(),
            "timed × inline has no execution path and must produce no wiring"
        );
        // With default knobs the derived timed pool covers the admission
        // bound: 32 demanded channels → 8 connections × 4 channels each.
        assert_eq!(
            transport::default_connections(timed_prewarm.channel_demand),
            8
        );
        // The timed demand is the larger of the two knobs, so an operator
        // who raises submit_concurrency above max_sessions is covered too.
        let knobs = Knobs {
            submit_concurrency: 64,
            ..Knobs::default()
        };
        assert_eq!(
            mode_wiring(ScheduleMode::Timed, SupplyDelivery::Prewarm, &knobs)
                .unwrap()
                .channel_demand,
            64
        );
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

    /// The dependency of appB in the mini archive whose drv path contains
    /// `needle`, as `(drv path, first output path)` — the supply stage's
    /// producer/outcome assertions key on these.
    fn app_b_dep(archive: &ReplayArchive, needle: &str) -> (String, String) {
        let units = load_units(archive).unwrap();
        let closures = load_closures(archive, &units).unwrap();
        let entry = closures
            .iter()
            .find(|d| d.job == "appB.x86_64-linux")
            .unwrap();
        let dep = entry
            .deps
            .iter()
            .find(|d| d.drv_path.contains(needle))
            .expect("mini archive has the dependency");
        (dep.drv_path.clone(), dep.output_paths[0].clone())
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn mini_campaign_end_to_end_and_resume() {
        let (_archive_dir, archive, archive_id) = open_mini_archive();
        let manifest = load_units(&archive).unwrap();
        let app_a = manifest
            .iter()
            .find(|m| m.job == "appA.x86_64-linux")
            .unwrap()
            .clone();
        let app_b = manifest
            .iter()
            .find(|m| m.job == "appB.x86_64-linux")
            .unwrap()
            .clone();
        let (_lib_drv, lib_out) = app_b_dep(&archive, "-libA-");
        let (_stdenv_drv, stdenv_out) = app_b_dep(&archive, "-stdenv-");

        // Prefetch-path upstream coverage: libA's output exists upstream,
        // the stdenv dep does not (its narinfo lookup misses → recorded
        // unavailable). Target truth never touches this map — it is baked
        // into the archive's outcomes.
        let mut narinfos = HashMap::new();
        narinfos.insert(lib_out.clone(), narinfo_text(&lib_out));

        // Submitter script: one appA+appB submit batch carrying both roots'
        // terminal outcomes in band (per-root results). The supply stage
        // never touches the submitter — its prefetch and uploads go through
        // the supply transport fake.
        let submitter = Arc::new(FakeSubmitter::default());
        let submit_build = "0193e4a2-7c1b-7d20-9b3a-00000000bbbb";
        let built_result = |drv: &str| PathOutcome {
            drv_path: drv.to_string(),
            status: build_status_name(BuildStatus::Built).into(),
            error_msg: String::new(),
            start_time: 0,
            stop_time: 0,
        };
        submitter.outcomes.lock().unwrap().push(Ok(BatchOutcome {
            build_id: Some(submit_build.into()),
            results: vec![built_result(&app_a.drv_path), built_result(&app_b.drv_path)],
            ..BatchOutcome::default()
        }));
        // Unscripted prefetch roots settle as Substituted, so libA's covered
        // output is delegated to the target cluster.
        let supply_transport = Arc::new(FakeSupplyTransport::default());

        // Empty rio-store: nothing is valid at the plan snapshot (so both
        // app units are attemptable, not cached-prior); collect's NAR read
        // then finds no info and records the outputs without hashes
        // (not-comparable), which does not affect the bucket.
        let state_dir = tempfile::tempdir().unwrap();
        let backends = || Backends {
            store: Arc::new(FakeStoreApi::default()),
            admin: Arc::new(NoLogsAdmin),
            cluster: Arc::new(HealthyCluster),
            submitter: submitter.clone(),
            supply_transport: Some(supply_transport.clone()),
            narinfo: Some(Arc::new(MapNarinfo(narinfos.clone()))),
            artifacts: None,
        };
        // The campaign also requests the regression-gate report policy so
        // this end-to-end run exercises gate.json: written to disk by the
        // report stage and mirrored in progress.json.
        let gated_spec = || {
            let mut spec = leaf_spec(&archive_id);
            spec.report
                .policies
                .push(spec::ReportPolicy::RegressionGate);
            spec.report.fail_on = spec::FailOn::Regression;
            spec
        };
        let state = StateDir::new(state_dir.path()).unwrap();
        run_with_backends(
            run_args(state_dir.path()),
            gated_spec(),
            state,
            archive.clone(),
            backends(),
        )
        .await
        .unwrap();

        // Final state assertions.
        let state = StateDir::new(state_dir.path()).unwrap();
        for marker in ["plan", "supply", "report"] {
            assert!(state.marker_done(marker), "marker {marker} set");
        }
        let records = latest_per_job(state.load_jsonl(StateFile::Results).unwrap());
        assert_eq!(
            records["appA.x86_64-linux"].verdict.as_deref(),
            Some("match-built")
        );
        assert_eq!(
            records["appB.x86_64-linux"].verdict.as_deref(),
            Some("match-built")
        );
        assert_eq!(
            records["divergentC.x86_64-linux"].disposition.as_deref(),
            Some("identity-divergent")
        );
        assert_eq!(
            records["kvmTest.x86_64-linux"].disposition.as_deref(),
            Some("filtered")
        );
        assert_eq!(
            records["libA.aarch64-linux"].disposition.as_deref(),
            Some("filtered")
        );
        // Supply outcomes replace the old warm dispositions: the covered
        // libA output is delegated to the target cluster (prefetch
        // substitution), the uncovered stdenv output is recorded
        // unavailable by the coverage probe, and the workload drv texts are
        // delivered by client upload before execution.
        let supply_entries: Vec<SupplyEntry> = state.load_jsonl(StateFile::Supply).unwrap();
        let outcome_of = |path: &str| -> Vec<&str> {
            supply_entries
                .iter()
                .filter(|entry| entry.path == path)
                .map(|entry| entry.outcome.as_str())
                .collect()
        };
        assert_eq!(outcome_of(&lib_out), vec![SUPPLY_OUTCOME_DELEGATED]);
        assert_eq!(outcome_of(&stdenv_out), vec![SUPPLY_OUTCOME_UNAVAILABLE]);
        assert!(
            supply_transport
                .uploaded_batches
                .lock()
                .unwrap()
                .iter()
                .flatten()
                .any(|path| path == &app_b.drv_path),
            "the supply stage uploads the workload drv texts"
        );
        let summary = std::fs::read_to_string(state.path("report/summary.md")).unwrap();
        assert!(summary.contains("Build-outcome parity"));
        assert!(state.path("buckets/match-built.jsonl").exists());
        let progress: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(state.path("progress.json")).unwrap())
                .unwrap();
        assert_eq!(progress["stage"], "done");
        // Regression gate: persisted on disk as report/gate.json with the
        // design's snake_case wire keys, and mirrored verbatim under
        // progress.json's "gate" key. Nothing in this campaign belongs to
        // the regression trip set, so the gate is untripped with no
        // contributing counts.
        let gate: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(state.path("report/gate.json")).unwrap())
                .unwrap();
        assert_eq!(gate["policy"], "regression-gate");
        assert_eq!(gate["fail_on"], "regression");
        assert_eq!(gate["tripped"], false);
        assert_eq!(gate["counts"], serde_json::json!({}));
        assert!(
            gate.get("failOn").is_none(),
            "gate.json keys are snake_case, never camelCase: {gate}"
        );
        assert_eq!(progress["gate"], gate);
        // campaign.json pins the archive and carries both exclusion
        // sources: the recorder-side exclusion counts from the archive and
        // the engine-side counts the comparability refresh re-derives from
        // the per-class vocabulary (the two filtered jobs roll up under
        // their disposition).
        let campaign: CampaignRecord = state.read_json("campaign.json").unwrap().unwrap();
        assert_eq!(campaign.archive.archive_id, archive_id);
        assert_eq!(
            campaign.comparability.eval_set,
            campaign.archive.archive_id_short
        );
        assert_eq!(campaign.comparability.excluded.get("eval-error"), Some(&1));
        assert_eq!(campaign.comparability.excluded.get("filtered"), Some(&2));
        // Archive provenance and campaign identity recorded at bootstrap:
        // the mini archive's manifest timestamp, its (positive) age relative
        // to the campaign, the recorder's one exclusion record, and the
        // scheduling/supply identity of a leaf timeless campaign.
        assert_eq!(
            campaign.comparability.archive_created_at.as_deref(),
            Some("2026-05-28T00:00:00Z")
        );
        assert!(
            campaign.comparability.archive_age_days.unwrap() > 0.0,
            "{:?}",
            campaign.comparability.archive_age_days
        );
        assert_eq!(campaign.comparability.exclusions_recorded, Some(1));
        assert_eq!(
            campaign.comparability.scheduling_mode.as_deref(),
            Some("timeless")
        );
        assert_eq!(
            campaign.comparability.supply_policy.as_deref(),
            Some("substituters")
        );

        // Resume: same state dir, no scripted submitter outcomes left → must
        // not submit anything new and must finish with identical buckets.
        let submitted_before = submitter.submitted.lock().unwrap().len();
        let state2 = StateDir::new(state_dir.path()).unwrap();
        run_with_backends(
            run_args(state_dir.path()),
            gated_spec(),
            state2,
            archive.clone(),
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
        assert_eq!(
            records2["appB.x86_64-linux"].verdict.as_deref(),
            Some("match-built")
        );

        // A different archive may not resume this campaign: the stored pin
        // refuses it before any stage runs. A one-unit archive is enough —
        // only its (different) identity matters.
        let other = tempfile::tempdir().unwrap();
        let other_id = {
            use crate::archive::schema::{
                Capabilities, RequestRecord, RequestTarget, Substituters, UnitRecord,
            };
            use crate::archive::writer::{ArchiveWriter, ManifestSeed};
            let drv = format!(
                "/nix/store/{}-other-1.0.drv",
                crate::run::archive_input::fake_hash("other-drv")
            );
            let out = format!(
                "/nix/store/{}-other-1.0",
                crate::run::archive_input::fake_hash("other-out")
            );
            let writer = ArchiveWriter::create(other.path()).unwrap();
            writer
                .add_drv(
                    &drv,
                    &format!(
                        r#"Derive([("out","{out}","","")],[],[],"x86_64-linux","/bin/sh",["-c","true"],[("out","{out}")])"#
                    ),
                )
                .unwrap();
            writer
                .write_units(&[UnitRecord {
                    drv: drv.clone(),
                    label: Some("other.x86_64-linux".to_string()),
                    system: Some("x86_64-linux".to_string()),
                    outputs: BTreeMap::from([("out".to_string(), out)]),
                    required_features: Vec::new(),
                    identity_divergent: false,
                }])
                .unwrap();
            writer
                .write_requests(&[RequestRecord {
                    session: 0,
                    offset_s: 0.0,
                    targets: vec![RequestTarget {
                        drv,
                        outputs: vec!["*".to_string()],
                    }],
                }])
                .unwrap();
            let stamp: jiff::Timestamp = "2026-05-28T00:00:00Z".parse().unwrap();
            writer
                .finalize(ManifestSeed {
                    created_at: stamp,
                    from: stamp,
                    to: stamp,
                    capabilities: Capabilities::default(),
                    substituters: Substituters {
                        relay: vec!["https://cache.example.org".to_string()],
                        target: Vec::new(),
                    },
                    fat: false,
                    provenance: serde_json::Map::new(),
                })
                .unwrap()
                .archive_id
        };
        assert_ne!(other_id, archive_id);
        let other_archive = Arc::new(ReplayArchive::open(other.path()).unwrap());
        let err = run_with_backends(
            run_args(state_dir.path()),
            leaf_spec(&other_id),
            StateDir::new(state_dir.path()).unwrap(),
            other_archive,
            backends(),
        )
        .await
        .unwrap_err();
        assert!(
            err.to_string().contains("campaign.json pins archive"),
            "{err:#}"
        );
    }

    /// The collect pass processes timed-dispatcher batches, not just the
    /// submit loop's: a settled timed batch with an armed interruption that
    /// the engine cancelled is classified (interruption-replayed) and marked
    /// processed instead of being silently skipped by the batch-kind filter,
    /// and its members are never re-offered to the timeless pending pool.
    #[tokio::test]
    async fn collect_pass_processes_timed_batches() {
        let dir = tempfile::tempdir().unwrap();
        let state = StateDir::new(dir.path()).unwrap();
        let drv = format!("/nix/store/{}-timed-app.drv", "f".repeat(32));
        let job = "timedApp.x86_64-linux";
        state
            .append_jsonl(
                StateFile::Batches,
                &model::BatchRecord {
                    batch_id: 41,
                    kind: BATCH_KIND_TIMED.to_string(),
                    jobs: vec![job.to_string()],
                    root_drvs: vec![drv.clone()],
                    est_nodes: 1,
                    build_id: None,
                    started_at: now_rfc3339(),
                    finished_at: Some(now_rfc3339()),
                    results: Vec::new(),
                    reasons: BTreeMap::new(),
                    stderr_tail: None,
                    engine_cancelled: true,
                    interruption_drvs: vec![drv.clone()],
                },
            )
            .unwrap();
        let contexts: HashMap<String, JobContext> = HashMap::from([(
            job.to_string(),
            JobContext {
                job: job.to_string(),
                system: "x86_64-linux".into(),
                drv_path: drv.clone(),
                outputs: BTreeMap::from([(
                    "out".to_string(),
                    format!("{}-out", drv.trim_end_matches(".drv")),
                )]),
                dep_drvs: HashSet::new(),
                expected_outcome: ExpectedOutcome::Built,
                expected_outputs: BTreeMap::new(),
                plan_not_attemptable: false,
                plan_snapshot_valid: false,
            },
        )]);
        let backends = CollectBackends {
            admin: Arc::new(NoLogsAdmin),
            store: Arc::new(FakeStoreApi::default()),
            artifacts: None,
        };
        let tracker = SubmitTracker::default();
        let mut processed = HashSet::new();
        let requeued = collect_pass_with(
            &state,
            &backends,
            &contexts,
            &tracker,
            &mut processed,
            &Knobs::default(),
            "leaf",
            "c-timed",
            None,
        )
        .await
        .unwrap();
        assert_eq!(requeued, 0, "timed batch members are never re-offered");
        assert!(
            processed.contains(&41),
            "the timed batch is marked processed"
        );
        let records = latest_per_job(state.load_jsonl(StateFile::Results).unwrap());
        assert_eq!(
            records[job].verdict.as_deref(),
            Some(Verdict::InterruptionReplayed.as_str())
        );
    }

    /// The poller's pause decision across scheduling modes: in timeless mode
    /// any backpressure condition (dispatch gap, queue depth, infra rate)
    /// pauses submission exactly as before; in timed mode none of them gates
    /// dispatch — the infra-rate condition becomes an abort recommendation
    /// instead.
    #[test]
    fn infra_pause_is_advisory_in_timed_mode() {
        // Timed: infra rate above the threshold recommends an abort, never a
        // pause; dispatch-gap and queue-depth conditions are advisory too.
        assert_eq!(
            pause_decision(ScheduleMode::Timed, false, false, true),
            (false, true)
        );
        assert_eq!(
            pause_decision(ScheduleMode::Timed, true, true, false),
            (false, false)
        );
        assert_eq!(
            pause_decision(ScheduleMode::Timed, false, false, false),
            (false, false)
        );
        // Timeless: unchanged — each condition pauses submission and no
        // abort is ever recommended.
        assert_eq!(
            pause_decision(ScheduleMode::Timeless, false, false, true),
            (true, false)
        );
        assert_eq!(
            pause_decision(ScheduleMode::Timeless, true, false, false),
            (true, false)
        );
        assert_eq!(
            pause_decision(ScheduleMode::Timeless, false, true, false),
            (true, false)
        );
        assert_eq!(
            pause_decision(ScheduleMode::Timeless, false, false, false),
            (false, false)
        );
    }

    /// Recorded offsets and timing values from an archive are sanitized at
    /// the conversion point: non-finite values can never reach the schedule
    /// math (where they would panic the duration conversion) and absurdly
    /// large finite offsets clamp to the cap instead of parking a request
    /// unreachably far in the future.
    #[test]
    fn recorded_inputs_sanitize_non_finite_offsets() {
        use crate::archive::schema::RequestTarget;

        let target = RequestTarget {
            drv: format!("/nix/store/{}-x.drv", "a".repeat(32)),
            outputs: vec!["*".to_string()],
        };
        let req = |offset_s: f64| RequestRecord {
            session: 7,
            offset_s,
            targets: vec![target.clone()],
        };
        assert_eq!(recorded_request_from(&req(f64::INFINITY)).offset_s, 0.0);
        assert_eq!(recorded_request_from(&req(f64::NAN)).offset_s, 0.0);
        assert_eq!(recorded_request_from(&req(-4.0)).offset_s, 0.0);
        assert_eq!(
            recorded_request_from(&req(1.0e30)).offset_s,
            MAX_RECORDED_OFFSET_S
        );
        assert_eq!(recorded_request_from(&req(12.5)).offset_s, 12.5);
        // The sanitized requests survive schedule construction (a raw +inf
        // offset would panic the due-time conversion).
        let requests: Vec<timeline::RecordedRequest> = [f64::INFINITY, 3.0]
            .iter()
            .map(|offset| recorded_request_from(&req(*offset)))
            .collect();
        let schedule = timeline::build_schedule(&requests, &|_, _| None, 1.0, None, true);
        assert_eq!(schedule.len(), 2);

        let outcome = |outcome, duration_s, stop_offset_s| OutcomeRecord {
            session: None,
            drv: target.drv.clone(),
            outcome,
            detail: None,
            duration_s,
            stop_offset_s,
            outputs: BTreeMap::new(),
        };
        // Non-finite durations/stop offsets are dropped rather than
        // poisoning the deadline and disconnect math; the interruption and
        // expected-built flags derive from the recorded outcome.
        let disconnected = recorded_timing_from(&outcome(
            ExpectedOutcome::Disconnected,
            Some(f64::NAN),
            Some(f64::INFINITY),
        ));
        assert_eq!(disconnected.duration_s, None);
        assert_eq!(disconnected.stop_offset_s, None);
        assert!(disconnected.interrupted && !disconnected.expected_built);
        let built =
            recorded_timing_from(&outcome(ExpectedOutcome::Built, Some(120.0), Some(130.0)));
        assert_eq!(built.duration_s, Some(120.0));
        assert_eq!(built.stop_offset_s, Some(130.0));
        assert!(!built.interrupted && built.expected_built);
        let cancelled = recorded_timing_from(&outcome(ExpectedOutcome::Cancelled, None, Some(2.0)));
        assert!(cancelled.interrupted && !cancelled.expected_built);
    }

    /// A timed campaign over an archive that does not declare the `timed`
    /// capability is refused at bootstrap, before any stage marker or
    /// campaign record is written.
    #[tokio::test]
    async fn timed_mode_requires_timed_capability() {
        let (_archive_dir, archive, archive_id) = open_mini_archive();
        assert!(!archive.capabilities().timed);
        let state_dir = tempfile::tempdir().unwrap();
        let mut spec = leaf_spec(&archive_id);
        spec.scheduling.mode = ScheduleMode::Timed;
        let backends = Backends {
            store: Arc::new(FakeStoreApi::default()),
            admin: Arc::new(NoLogsAdmin),
            cluster: Arc::new(HealthyCluster),
            submitter: Arc::new(FakeSubmitter::default()),
            supply_transport: Some(Arc::new(FakeSupplyTransport::default())),
            narinfo: Some(Arc::new(MapNarinfo(HashMap::new()))),
            artifacts: None,
        };
        let err = run_with_backends(
            run_args(state_dir.path()),
            spec,
            StateDir::new(state_dir.path()).unwrap(),
            archive,
            backends,
        )
        .await
        .unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("timed") && msg.contains("capability"), "{msg}");
        let state = StateDir::new(state_dir.path()).unwrap();
        for marker in ["plan", "supply", "report"] {
            assert!(
                !state.marker_done(marker),
                "marker {marker} must not be set"
            );
        }
        assert!(
            state
                .read_json::<CampaignRecord>("campaign.json")
                .unwrap()
                .is_none(),
            "no campaign record is written before the refusal"
        );
    }

    /// A missing narinfo probe (no probeable public-HTTPS substituter in
    /// the archive) fails only at its single point of use — the leaf-mode
    /// warm-set coverage probe — with an error naming the requirement.
    /// Bootstrap and the plan stage run normally first: the archive itself
    /// is valid, only this campaign shape needs the probe.
    #[tokio::test]
    async fn leaf_campaign_without_probeable_substituter_errors_at_the_probe() {
        let (_archive_dir, archive, archive_id) = open_mini_archive();
        let state_dir = tempfile::tempdir().unwrap();
        let backends = Backends {
            store: Arc::new(FakeStoreApi::default()),
            admin: Arc::new(NoLogsAdmin),
            cluster: Arc::new(HealthyCluster),
            submitter: Arc::new(FakeSubmitter::default()),
            supply_transport: Some(Arc::new(FakeSupplyTransport::default())),
            narinfo: None,
            artifacts: None,
        };
        let err = run_with_backends(
            run_args(state_dir.path()),
            leaf_spec(&archive_id),
            StateDir::new(state_dir.path()).unwrap(),
            archive,
            backends,
        )
        .await
        .unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("warm-set upstream-coverage probe") && msg.contains("no usable entry"),
            "{msg}"
        );
        // The plan stage completed (the archive is fine); only the supply
        // stage's probe was unable to run.
        let state = StateDir::new(state_dir.path()).unwrap();
        assert!(state.marker_done("plan"));
        assert!(!state.marker_done("supply"));
    }

    /// A timed campaign over a timed-capable archive with no recorded
    /// interruptions degrades `replay_interruptions` to off: the forced-off
    /// knob is recorded as the `replay-interruptions-disabled`
    /// low-confidence flag in campaign.json, and the dispatcher arms no
    /// interruption on any batch.
    #[tokio::test(flavor = "multi_thread")]
    async fn replay_interruptions_forced_off_without_interruption_records() {
        let archive_dir = tempfile::tempdir().unwrap();
        let built = write_mini_timed_archive(archive_dir.path(), false);
        let archive = Arc::new(ReplayArchive::open(archive_dir.path()).unwrap());
        let manifest = load_units(&archive).unwrap();
        let app_a = manifest
            .iter()
            .find(|m| m.job == "appA.x86_64-linux")
            .unwrap()
            .clone();
        let app_b = manifest
            .iter()
            .find(|m| m.job == "appB.x86_64-linux")
            .unwrap()
            .clone();
        let (_lib_drv, lib_out) = app_b_dep(&archive, "-libA-");
        let mut narinfos = HashMap::new();
        narinfos.insert(lib_out.clone(), narinfo_text(&lib_out));

        // Both recorded requests settle Built in band, keyed by root drv so
        // the concurrent timed dispatch tasks cannot pop each other's script.
        let built_result = |drv: &str| PathOutcome {
            drv_path: drv.to_string(),
            status: build_status_name(BuildStatus::Built).into(),
            error_msg: String::new(),
            start_time: 0,
            stop_time: 0,
        };
        let submitter = Arc::new(KeyedSubmitter::default());
        for drv in [&app_a.drv_path, &app_b.drv_path] {
            submitter.outcomes.lock().unwrap().insert(
                drv.clone(),
                BatchOutcome {
                    results: vec![built_result(drv)],
                    ..BatchOutcome::default()
                },
            );
        }

        let state_dir = tempfile::tempdir().unwrap();
        let mut spec = leaf_spec(&built.archive_id);
        spec.scheduling.mode = ScheduleMode::Timed;
        spec.knobs.speedup = 1000.0;
        let backends = Backends {
            store: Arc::new(FakeStoreApi::default()),
            admin: Arc::new(NoLogsAdmin),
            cluster: Arc::new(HealthyCluster),
            submitter: submitter.clone(),
            supply_transport: Some(Arc::new(FakeSupplyTransport::default())),
            narinfo: Some(Arc::new(MapNarinfo(narinfos))),
            artifacts: None,
        };
        run_with_backends(
            run_args(state_dir.path()),
            spec,
            StateDir::new(state_dir.path()).unwrap(),
            archive,
            backends,
        )
        .await
        .unwrap();

        let state = StateDir::new(state_dir.path()).unwrap();
        let campaign: CampaignRecord = state.read_json("campaign.json").unwrap().unwrap();
        assert!(
            campaign
                .comparability
                .low_confidence
                .iter()
                .any(|flag| flag == report::FLAG_REPLAY_INTERRUPTIONS_DISABLED),
            "{:?}",
            campaign.comparability.low_confidence
        );
        let batches: Vec<model::BatchRecord> = state.load_jsonl(StateFile::Batches).unwrap();
        assert!(!batches.is_empty());
        assert!(
            batches.iter().all(|b| b.interruption_drvs.is_empty()),
            "no interruption is armed when the archive records none"
        );
        let records = latest_per_job(state.load_jsonl(StateFile::Results).unwrap());
        assert_eq!(
            records["appA.x86_64-linux"].verdict.as_deref(),
            Some("match-built")
        );
        assert_eq!(
            records["appB.x86_64-linux"].verdict.as_deref(),
            Some("match-built")
        );
    }

    /// Timed campaign end to end over a mini timed archive: the supply stage
    /// runs, both recorded requests are dispatched on the recorded schedule,
    /// the recorded disconnect is replayed (engine cancellation classified
    /// `interruption-replayed`), and the report renders.
    #[tokio::test(flavor = "multi_thread")]
    async fn mini_timed_campaign_end_to_end() {
        let archive_dir = tempfile::tempdir().unwrap();
        let built = write_mini_timed_archive(archive_dir.path(), true);
        let archive = Arc::new(ReplayArchive::open(archive_dir.path()).unwrap());
        let manifest = load_units(&archive).unwrap();
        let app_a = manifest
            .iter()
            .find(|m| m.job == "appA.x86_64-linux")
            .unwrap()
            .clone();
        let app_b = manifest
            .iter()
            .find(|m| m.job == "appB.x86_64-linux")
            .unwrap()
            .clone();
        let (_lib_drv, lib_out) = app_b_dep(&archive, "-libA-");
        let mut narinfos = HashMap::new();
        narinfos.insert(lib_out.clone(), narinfo_text(&lib_out));

        // appA settles Built in band; appB's submission is engine-cancelled,
        // reproducing the recorded disconnect.
        let submitter = Arc::new(KeyedSubmitter::default());
        submitter.outcomes.lock().unwrap().insert(
            app_a.drv_path.clone(),
            BatchOutcome {
                build_id: Some("0193e4a2-7c1b-7d20-9b3a-00000000cccc".into()),
                results: vec![PathOutcome {
                    drv_path: app_a.drv_path.clone(),
                    status: build_status_name(BuildStatus::Built).into(),
                    error_msg: String::new(),
                    start_time: 0,
                    stop_time: 0,
                }],
                ..BatchOutcome::default()
            },
        );
        submitter.outcomes.lock().unwrap().insert(
            app_b.drv_path.clone(),
            BatchOutcome {
                engine_cancelled: true,
                ..BatchOutcome::default()
            },
        );

        // The supply transport is shared between the supply stage and the
        // dispatcher's pre-submission top-up. Two scripted refusals make the
        // prewarm pass miss appA's derivation text (refusal + retry both
        // refused), so the top-up has a genuine prewarm miss to deliver at
        // dispatch time.
        let supply_transport = Arc::new(FakeSupplyTransport::default());
        supply_transport
            .refusals
            .lock()
            .unwrap()
            .insert(app_a.drv_path.clone(), 2);

        let state_dir = tempfile::tempdir().unwrap();
        let mut spec = leaf_spec(&built.archive_id);
        spec.scheduling.mode = ScheduleMode::Timed;
        spec.knobs.speedup = 1000.0;
        let backends = Backends {
            store: Arc::new(FakeStoreApi::default()),
            admin: Arc::new(NoLogsAdmin),
            cluster: Arc::new(HealthyCluster),
            submitter: submitter.clone(),
            supply_transport: Some(supply_transport.clone()),
            narinfo: Some(Arc::new(MapNarinfo(narinfos))),
            artifacts: None,
        };
        run_with_backends(
            run_args(state_dir.path()),
            spec,
            StateDir::new(state_dir.path()).unwrap(),
            archive,
            backends,
        )
        .await
        .unwrap();

        let state = StateDir::new(state_dir.path()).unwrap();
        assert!(state.marker_done("supply"), "supply stage marker set");
        // The prewarm pass missed appA's derivation text (scripted refusal),
        // and the dispatcher's pre-submission top-up delivered it before the
        // request was submitted: the supply journal carries both settlements
        // and the target ended up with every workload derivation text.
        let supply_entries: Vec<SupplyEntry> = state.load_jsonl(StateFile::Supply).unwrap();
        let outcomes_for = |path: &str| -> Vec<&str> {
            supply_entries
                .iter()
                .filter(|entry| entry.path == path)
                .map(|entry| entry.outcome.as_str())
                .collect()
        };
        assert!(
            outcomes_for(&app_a.drv_path).contains(&SUPPLY_OUTCOME_REFUSED),
            "{supply_entries:?}"
        );
        assert!(
            outcomes_for(&app_a.drv_path).contains(&SUPPLY_OUTCOME_DELIVERED),
            "{supply_entries:?}"
        );
        {
            let valid = supply_transport.valid.lock().unwrap();
            for drv in [&app_a.drv_path, &app_b.drv_path] {
                assert!(
                    valid.contains(drv.as_str()),
                    "the top-up delivered {drv} after the prewarm miss"
                );
            }
        }
        // One dispatch entry per recorded request; the appB request carries
        // the armed-and-fired interruption.
        let dispatch: Vec<DispatchEntry> = state.load_jsonl(StateFile::Dispatch).unwrap();
        assert_eq!(dispatch.len(), 2);
        assert!(
            dispatch
                .iter()
                .any(|entry| entry.interruption_armed && entry.interruption_fired),
            "{dispatch:?}"
        );
        // Every submission is a timed batch; the armed one names the drv.
        let batches: Vec<model::BatchRecord> = state.load_jsonl(StateFile::Batches).unwrap();
        assert!(!batches.is_empty());
        assert!(batches.iter().all(|b| b.kind == BATCH_KIND_TIMED));
        assert!(
            batches
                .iter()
                .any(|b| b.engine_cancelled && b.interruption_drvs == vec![app_b.drv_path.clone()])
        );
        // Exactly one unit matched its recorded build and one reproduced its
        // recorded interruption.
        let records = latest_per_job(state.load_jsonl(StateFile::Results).unwrap());
        let verdict_count = |verdict: &str| {
            records
                .values()
                .filter(|record| record.verdict.as_deref() == Some(verdict))
                .count()
        };
        assert_eq!(
            records["appA.x86_64-linux"].verdict.as_deref(),
            Some("match-built")
        );
        assert_eq!(
            records["appB.x86_64-linux"].verdict.as_deref(),
            Some(Verdict::InterruptionReplayed.as_str())
        );
        assert_eq!(verdict_count("match-built"), 1);
        assert_eq!(verdict_count(Verdict::InterruptionReplayed.as_str()), 1);
        // The dispatcher's run statistics are persisted for the report.
        let stats: timeline::TimedRunStats = state
            .read_json("timed-stats.json")
            .unwrap()
            .expect("timed-stats.json written by the timed arm");
        assert_eq!(stats.requests_total, 2);
        assert_eq!(stats.dispatched, 2);
        assert_eq!(stats.interruptions_replayed, 1);
        assert_eq!(stats.submission_failures, 0);
        assert!(!stats.timing_degraded);
        // The final progress document carries the supply and timed summary
        // blocks (with the upload-throughput field present even when the
        // fake transport uploaded nothing) and the abort recommendation.
        let progress: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(state.path("progress.json")).unwrap())
                .unwrap();
        assert_eq!(progress["stage"], "done");
        assert!(progress["supply"].is_object(), "{progress}");
        assert!(
            progress["supply"].get("uploadMibPerS").is_some(),
            "{progress}"
        );
        assert_eq!(progress["timed"]["dispatched"], 2);
        assert_eq!(progress["timed"]["interruptionsReplayed"], 1);
        assert_eq!(progress["abortRecommended"], false);
        // The plan-time closure-graph memory measurement is in the persisted
        // plan counts.
        let campaign: CampaignRecord = state.read_json("campaign.json").unwrap().unwrap();
        let plan_counts = &campaign.plan.as_ref().unwrap().counts;
        assert!(
            plan_counts.contains_key(PLAN_COUNT_RSS_PEAK),
            "{plan_counts:?}"
        );
        let summary = std::fs::read_to_string(state.path("report/summary.md")).unwrap();
        assert!(summary.contains("Build-outcome parity"));
        assert!(summary.contains("## Supply"), "{summary}");
        assert!(summary.contains("## Timed dispatch"), "{summary}");
    }

    /// Transport-error path end to end: the first submission fails
    /// engine-side (channel open refused), the batch is recorded with the
    /// error text and no in-band results, the jobs are re-offered, and the
    /// resubmission's in-band Built results drain the campaign.
    ///
    /// Nothing in the prefetch set exists upstream here, so the supply
    /// stage's prefetch arm has nothing to delegate and the stage reduces to
    /// drv-text uploads through the supply transport fake.
    #[tokio::test(flavor = "multi_thread")]
    async fn transport_error_batches_are_reoffered_and_drain() {
        let (_archive_dir, archive, archive_id) = open_mini_archive();
        let manifest = load_units(&archive).unwrap();
        let app_a = manifest
            .iter()
            .find(|m| m.job == "appA.x86_64-linux")
            .unwrap()
            .clone();
        let app_b = manifest
            .iter()
            .find(|m| m.job == "appB.x86_64-linux")
            .unwrap()
            .clone();

        // FakeSubmitter pops from the BACK: the FIRST submission fails at the
        // transport (engine-side error), the SECOND carries both roots'
        // terminal outcomes in band.
        let submitter = Arc::new(FakeSubmitter::default());
        let submit_build = "0193e4a2-7c1b-7d20-9b3a-00000000cccc";
        let built_result = |drv: &str| PathOutcome {
            drv_path: drv.to_string(),
            status: build_status_name(BuildStatus::Built).into(),
            error_msg: String::new(),
            start_time: 0,
            stop_time: 0,
        };
        submitter.outcomes.lock().unwrap().push(Ok(BatchOutcome {
            build_id: Some(submit_build.into()),
            results: vec![built_result(&app_a.drv_path), built_result(&app_b.drv_path)],
            ..BatchOutcome::default()
        }));
        submitter.outcomes.lock().unwrap().push(Err(anyhow::anyhow!(
            "channel open failed: connection refused"
        )));

        let state_dir = tempfile::tempdir().unwrap();
        let state = StateDir::new(state_dir.path()).unwrap();
        run_with_backends(
            run_args(state_dir.path()),
            leaf_spec(&archive_id),
            state,
            archive.clone(),
            Backends {
                store: Arc::new(FakeStoreApi::default()),
                admin: Arc::new(NoLogsAdmin),
                cluster: Arc::new(HealthyCluster),
                submitter: submitter.clone(),
                supply_transport: Some(Arc::new(FakeSupplyTransport::default())),
                narinfo: Some(Arc::new(MapNarinfo(HashMap::new()))),
                artifacts: None,
            },
        )
        .await
        .unwrap();

        // The jobs were attempted at least twice (transport failure, then
        // the successful resubmission) and end match-built.
        let records = latest_per_job(
            StateDir::new(state_dir.path())
                .unwrap()
                .load_jsonl(StateFile::Results)
                .unwrap(),
        );
        let app_record = &records["appB.x86_64-linux"];
        assert_eq!(app_record.verdict.as_deref(), Some("match-built"));
        assert!(app_record.attempts >= 1, "{app_record:?}");
        assert_eq!(
            records["appA.x86_64-linux"].verdict.as_deref(),
            Some("match-built")
        );

        // Two build-path submissions were recorded (the supply stage's
        // prefetch and uploads never touch batches.jsonl): the first carries
        // the engine submission error and an empty results array, the second
        // the in-band results.
        let state = StateDir::new(state_dir.path()).unwrap();
        let mut batches: Vec<model::BatchRecord> = state
            .load_jsonl::<model::BatchRecord>(StateFile::Batches)
            .unwrap();
        batches.sort_by_key(|b| b.batch_id);
        assert!(
            batches.iter().all(|b| b.kind == BATCH_KIND_SUBMIT),
            "{batches:?}"
        );
        assert!(batches.len() >= 2, "{batches:?}");
        assert_eq!(batches[0].build_id, None);
        assert!(batches[0].results.is_empty());
        assert!(
            batches[0]
                .stderr_tail
                .as_deref()
                .unwrap_or_default()
                .contains("channel open failed: connection refused"),
            "{:?}",
            batches[0].stderr_tail
        );
        assert_eq!(batches[1].build_id.as_deref(), Some(submit_build));
        assert_eq!(batches[1].results.len(), 2);
        let result_drvs: Vec<&str> = batches[1]
            .results
            .iter()
            .map(|r| r.drv_path.as_str())
            .collect();
        assert!(result_drvs.contains(&app_a.drv_path.as_str()));
        assert!(result_drvs.contains(&app_b.drv_path.as_str()));
        assert!(
            batches[1]
                .results
                .iter()
                .all(|r| r.status == build_status_name(BuildStatus::Built))
        );
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
            expected_outcome: ExpectedOutcome::Built,
            expected_outputs: BTreeMap::new(),
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
            "c-stall",
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
            "c-stall",
        )
        .await
        .unwrap();
        let records = latest_per_job(state.load_jsonl(StateFile::Results).unwrap());
        assert_eq!(
            records[active_job].verdict.as_deref(),
            Some("infra-indeterminate")
        );
        assert_eq!(records[active_job].failure_cause.as_deref(), Some("infra"));
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
            "c-stall",
        )
        .await
        .unwrap();
        let records = latest_per_job(state.load_jsonl(StateFile::Results).unwrap());
        assert_eq!(
            records[queued_job].verdict.as_deref(),
            Some("infra-indeterminate")
        );
        assert_eq!(
            records[queued_job].signature.as_deref(),
            Some("stalled-queued")
        );
    }

    /// Deadline-partial path: write_not_attempted_records backfills every
    /// in-scope job with no record yet, so bucket counts sum to in-scope.
    #[test]
    fn partial_report_backfills_not_attempted_to_in_scope_total() {
        let (_archive_dir, archive, _archive_id) = open_mini_archive();
        let manifest = load_units(&archive).unwrap();
        let state_dir = tempfile::tempdir().unwrap();
        let state = StateDir::new(state_dir.path()).unwrap();

        // In scope: appA + appB. appA already has a terminal plan-time
        // record (not-attemptable); appB never produced one (deadline hit
        // first).
        let plan = PlanOutput {
            in_scope: vec!["appA.x86_64-linux".into(), "appB.x86_64-linux".into()],
            ..PlanOutput::default()
        };
        let app_a = manifest
            .iter()
            .find(|m| m.job == "appA.x86_64-linux")
            .unwrap();
        state
            .append_jsonl(
                StateFile::Results,
                &JobRecord {
                    job: app_a.job.clone(),
                    system: app_a.system.clone(),
                    drv_path: app_a.drv_path.clone(),
                    mode: "leaf".into(),
                    attempts: 0,
                    build_ids: vec![],
                    rio: model::RioSide {
                        outcome: Disposition::NotAttemptable.as_str().into(),
                        ..Default::default()
                    },
                    expected: model::ExpectedSide {
                        outcome: ExpectedOutcome::Unknown.as_str().into(),
                        ..Default::default()
                    },
                    nar_compare: BTreeMap::new(),
                    verdict: None,
                    disposition: Some(Disposition::NotAttemptable.as_str().into()),
                    cascaded: false,
                    failure_cause: None,
                    flaky: false,
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
        assert_eq!(app.disposition.as_deref(), Some("not-attempted"));
        assert_eq!(app.verdict, None);
        assert_eq!(app.rio.outcome, "not-attempted");
        assert!(app.build_ids.is_empty() && app.rio.exec_id.is_none());
        // Per-class counts sum to in-scope: the partial report is complete
        // over the scope.
        let agg = report::aggregate(&records);
        assert_eq!(
            agg.verdict_counts.values().sum::<usize>()
                + agg.disposition_counts.values().sum::<usize>(),
            plan.in_scope.len()
        );
        // Idempotent: nothing more to write on a second call.
        assert_eq!(
            write_not_attempted_records(&state, &manifest, &plan, "leaf", &records).unwrap(),
            0
        );
    }
}
