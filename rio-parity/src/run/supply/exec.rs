//! Supply execution arms — the transport seam and the machinery that
//! delivers planned supply to the target cluster.
//!
//! The parent module plans (what each closure needs and where it comes
//! from); this module moves the bytes:
//!
//! - [`SupplyTransport`] is the seam over the daemon operations the supply
//!   stage needs (validity probes, batch and streamed client uploads,
//!   prefetch builds), implemented for production by
//!   [`PoolSupplyTransport`] over the gateway SSH transport and by a
//!   scripted fake in tests.
//! - [`run_supply_stage`] is the stage orchestrator: it classifies the
//!   prefetch set (already-present / unavailable / prefetchable), runs the
//!   prefetch arm, drives the upload ladder over the workload union closure,
//!   and gates execution on the prefetch shortfall (a shortfall above
//!   `prefetch_shortfall_pause_pct` writes the campaign PAUSE file before
//!   the execution clock starts).
//! - [`prewarm_uploads`] pushes one upload plan in reference-safe order:
//!   large NARs streamed individually, everything else as multi-path
//!   upload batches fanned out over a small worker pool, with a single
//!   refusal retry on a fresh channel and a run-wide circuit breaker that
//!   stops dialing a gateway that is clearly gone.
//! - [`prefetch_arm`] delegates target-substituter-covered paths to the
//!   target cluster itself via prefetch builds over the prefetch tenant.
//! - [`topup_for_roots`] is the per-submission gap top-up (the prewarm-miss
//!   fallback and the inline-delivery path): probe, plan, and upload only
//!   what the given roots' closure still misses.
//!
//! Every per-path outcome is appended to `supply.jsonl` as a
//! [`SupplyEntry`] using the shared vocabulary constants. Per-path and
//! per-batch problems degrade — they are recorded and the affected paths
//! fall back to the per-request top-up — only systemic failures abort.
//!
//! Substituter URLs that originate outside the engine (the campaign spec's
//! `supply.target_substituters`, the archive manifest's substituter lists)
//! are admitted only through [`admit_substituter`] /
//! [`SupplyContext::add_relay_substituters`], which apply the public-cache
//! guard before [`Substituter::parse`] ever sees the URL.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use anyhow::{Context as _, Result, anyhow};
use async_trait::async_trait;
use futures_util::StreamExt;
use futures_util::future::join_all;
use rio_nix::narinfo::NarInfo;
use rio_nix::protocol::build::BuildStatus;
use rio_nix::protocol::client::{NarPayload, StoreEntry};
use rio_nix::protocol::pathinfo::ValidPathInfo;
use serde::{Deserialize, Serialize};
use tokio::sync::RwLock;

use crate::archive::reader::ReplayArchive;
use crate::run::model::{
    PathOutcome, SUPPLY_MECHANISM_DELEGATE, SUPPLY_MECHANISM_NONE, SUPPLY_MECHANISM_UPLOAD_BATCH,
    SUPPLY_MECHANISM_UPLOAD_STREAM, SUPPLY_OUTCOME_ALREADY_PRESENT, SUPPLY_OUTCOME_DELEGATED,
    SUPPLY_OUTCOME_DELIVERED, SUPPLY_OUTCOME_FAILED, SUPPLY_OUTCOME_REFUSED,
    SUPPLY_OUTCOME_UNAVAILABLE, SUPPLY_SOURCE_EMBEDDED, SUPPLY_SOURCE_NONE, SUPPLY_SOURCE_RELAY,
    SUPPLY_SOURCE_TARGET_SUBSTITUTER, SupplyEntry, build_status_from_name, build_status_name,
    now_rfc3339,
};
use crate::run::spec::{Knobs, SupplyDelivery, SupplyDependencies};
use crate::run::state::{StateDir, StateFile};
use crate::run::transport::{DaemonChannel, GatewayPool, TransportError};
use crate::substituter::Substituter;

use super::{
    ClaimOutcome, PathSource, UploadClaims, UploadItem, UploadPayload, UploadPlan, plan_uploads,
    resolve_source, split_batches, topo_levels, walk_closure,
};

/// Per-operation deadline for individually streamed (large) uploads. Probes
/// and batch uploads use the spec's `op_timeout_secs`; a multi-GB NAR
/// legitimately needs longer.
const LARGE_UPLOAD_TIMEOUT: Duration = Duration::from_secs(600);

/// Failure detail recorded for uploads abandoned after the gateway circuit
/// breaker trips.
const GATEWAY_UNREACHABLE: &str = "gateway unreachable; not retried";

/// One store-path payload ready to send (path info plus materialized bytes
/// or a streaming reader).
pub struct PreparedEntry {
    /// Full store path being uploaded.
    pub store_path: String,
    /// Wire path-info sent ahead of the NAR bytes.
    pub info: ValidPathInfo,
    /// The NAR serialization (in-memory bytes or a streaming reader).
    pub nar: NarPayload,
}

/// Errors the upload arms distinguish: a daemon refusal (retry once on a
/// fresh channel, then mark refused) vs. everything else (failed).
#[derive(Debug, thiserror::Error)]
pub enum SupplyTransportError {
    /// The daemon refused the upload (or refused it in a way that raced
    /// session teardown).
    #[error("daemon refused: {0}")]
    Refused(String),
    /// Transport, timeout, or channel-open failure.
    #[error(transparent)]
    Other(#[from] anyhow::Error),
}

/// The daemon/cluster operations the supply stage needs, behind a seam so
/// the planner and stage logic are testable without SSH.
#[async_trait]
pub trait SupplyTransport: Send + Sync {
    /// QueryValidPaths over the build tenant; returns the subset already valid.
    async fn query_valid(&self, paths: &[String]) -> anyhow::Result<BTreeSet<String>>;
    /// AddMultipleToStore of the given entries on one fresh channel.
    async fn upload_batch(&self, entries: Vec<PreparedEntry>) -> Result<(), SupplyTransportError>;
    /// AddToStoreNar (streamed) of one large entry on one fresh channel.
    async fn upload_streamed(&self, entry: PreparedEntry) -> Result<(), SupplyTransportError>;
    /// BuildPathsWithResults of prefetch roots over the prefetch tenant;
    /// per-root results in submission order.
    async fn prefetch_build(
        &self,
        roots: &[String],
        timeout: Duration,
    ) -> anyhow::Result<Vec<PathOutcome>>;
}

/// Pre-submission supply hook: deliver whatever the given roots' closures
/// still miss before the request is submitted (the prewarm-miss fallback and
/// the inline-delivery path). The production implementation wraps
/// [`topup_for_roots`] over the campaign's supply context.
#[async_trait]
pub trait PreSubmitSupply: Send + Sync {
    /// Top up the target store for the given root derivations.
    async fn topup(&self, roots: &[String]) -> anyhow::Result<()>;
}

/// What the supply stage did, for progress.json and the final report.
///
/// [`prewarm_uploads`] fills the upload-related fields (delivered, refused,
/// failed, bytes, throughput); the stage orchestrator merges in the
/// prefetch, already-present, and unavailable accounting.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct SupplyStageReport {
    /// Paths the supply policy planned to prefetch via the target cluster.
    pub planned_prefetch: usize,
    /// Planned prefetch paths whose prefetch did not settle as delegated.
    pub prefetch_missing: usize,
    /// Paths delivered by an engine upload (batch or streamed).
    pub delivered: usize,
    /// Paths the target cluster supplied itself (prefetch substitution or
    /// fallback build).
    pub delegated: usize,
    /// Paths already valid on the target before the stage sent anything.
    pub already_present: usize,
    /// Upload attempts the daemon refused even after the fresh-channel retry.
    pub refused: usize,
    /// Paths no source could provide.
    pub unavailable: usize,
    /// Delivery attempts that failed (transport, relay fetch, prefetch build).
    pub failed: usize,
    /// Total uncompressed NAR bytes uploaded by the engine.
    pub uploaded_bytes: u64,
    /// Wall-clock seconds spent in the upload arms.
    pub upload_secs: f64,
    /// Sustained engine upload throughput in MiB/s; `None` when nothing was
    /// uploaded.
    pub upload_mib_per_s: Option<f64>,
    /// Substituter narinfo-probe failure counts by cache URL.
    pub probe_errors: BTreeMap<String, u64>,
    /// Prefetch shortfall percentage (missing / planned × 100); `None` when
    /// nothing was planned for prefetch.
    pub shortfall_pct: Option<f64>,
}

/// What the prefetch arm settled, per path.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PrefetchArmStats {
    /// Paths the target cluster delegated (substituted or built as fallback).
    pub delegated: usize,
    /// Paths whose prefetch build failed or returned no result.
    pub failed: usize,
}

/// Inputs to [`run_supply_stage`], assembled by the campaign orchestrator
/// from the plan output, the open replay archive, and the campaign spec.
pub struct SupplyInputs {
    /// Output paths of the attemptable workload units (never supplied — the
    /// campaign exists to measure the target building them).
    pub workload_outputs: BTreeSet<String>,
    /// Drv store paths of the attemptable workload units; the upload ladder
    /// plans over the union of their dependency closures.
    pub workload_drvs: BTreeSet<String>,
    /// Paths the supply policy wants present before measurement, with their
    /// producing drvs (the dependency-output prefetch set + producer map).
    pub prefetch_paths: BTreeMap<String, Option<String>>,
    /// Paths already valid in the target store at the plan snapshot.
    pub prior_valid: BTreeSet<String>,
    /// Upstream coverage already known per path (the coverage probe's
    /// hydra.jsonl `found` entries); the supply stage's own narinfo probes
    /// extend it for the upload ladder.
    pub target_coverage: BTreeSet<String>,
    /// The open replay archive (production always passes it; `None` degrades
    /// the stage to the prefetch arm only and exists for unit tests).
    pub archive: Option<Arc<ReplayArchive>>,
    /// Operator-provided target substituter URLs
    /// (`spec.supply.target_substituters`). Every entry must pass the
    /// public-cache guard or the stage aborts; an empty list falls back to
    /// the archive manifest's advisory target list, which — being archive
    /// input — degrades with a warning instead.
    pub target_substituters: Vec<String>,
    /// Relay substituter URLs accepted from the archive manifest (https/s3
    /// only; rejected entries degrade with a warning).
    pub relay_substituters: Vec<String>,
    /// Effective dependency policy applied by the source ladder.
    pub dependencies: SupplyDependencies,
    /// When planned supply is delivered relative to execution.
    pub delivery: SupplyDelivery,
}

/// Parse a substituter URL provided by the campaign spec
/// (`supply.target_substituters`) or by the archive manifest, applying the
/// public-cache guard BEFORE the URL reaches [`Substituter::parse`]: only
/// public-host `https://` caches and `s3://` buckets are accepted.
/// `Substituter::parse` itself stays permissive (its plain-http and loopback
/// support is needed by offline tests and dev flows) — the trust boundary
/// for config- and archive-provided URLs is here.
pub async fn admit_substituter(url: &str) -> Result<Substituter> {
    crate::nixcache::validate_supply_substituter(url)?;
    Substituter::parse(url).await
}

/// Per-tenant SSH private-key path under the campaign's key directory
/// (`<ssh_key_dir>/<tenant>`), used to dial the gateway as that tenant.
///
/// The tenant name becomes a single path component under the key directory,
/// so it is restricted to a plain file-name alphabet (ASCII alphanumerics,
/// `-`, `_`); anything else — path separators, `..`, an empty name — is
/// rejected so a crafted tenant value can never point the key path outside
/// the directory. The key file itself is deliberately not checked for
/// existence here: this only derives the path.
pub fn tenant_key_path(ssh_key_dir: &Path, tenant: &str) -> Result<PathBuf> {
    anyhow::ensure!(
        !tenant.is_empty()
            && tenant
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_'),
        "invalid tenant name {tenant:?}: must be non-empty and contain only ASCII \
         alphanumerics, '-' or '_'"
    );
    Ok(ssh_key_dir.join(tenant))
}

/// Run-wide supply-ladder context shared by the upload and top-up arms:
/// the probe results the source ladder consumes, the dependency policy, the
/// admitted relay substituters, and the evolving set of paths known valid on
/// the target.
pub struct SupplyContext {
    /// Output paths of workload units (never supplied).
    pub workload_outputs: BTreeSet<String>,
    /// Paths a target-cluster substituter covers (ladder rung 1).
    pub target_coverage: BTreeSet<String>,
    /// Full store path → (relay substituter canonical URL, narinfo) for
    /// relay-able paths (ladder rung 3).
    pub relay_narinfos: HashMap<String, (String, NarInfo)>,
    /// Dependency policy applied by the source ladder.
    pub dependencies: SupplyDependencies,
    /// Paths known valid on the target (probed valid + successfully
    /// uploaded); the upload arms extend it as uploads land.
    pub target_valid: RwLock<BTreeSet<String>>,
    /// Admitted relay substituters keyed by their canonical URL. Populated
    /// only through [`SupplyContext::add_relay_substituters`], which applies
    /// the public-cache guard, so relay fetches can never reach a URL that
    /// bypassed it.
    relay: HashMap<String, Substituter>,
}

impl SupplyContext {
    /// Empty context under the given dependency policy.
    pub fn new(dependencies: SupplyDependencies) -> Self {
        Self {
            workload_outputs: BTreeSet::new(),
            target_coverage: BTreeSet::new(),
            relay_narinfos: HashMap::new(),
            dependencies,
            target_valid: RwLock::new(BTreeSet::new()),
            relay: HashMap::new(),
        }
    }

    /// Admit relay substituter URLs taken from the archive manifest. Each
    /// URL passes the public-cache guard ([`admit_substituter`]) before it
    /// is parsed; URLs that fail it are skipped with a warning (the affected
    /// paths degrade to unavailable rather than aborting the stage). Returns
    /// the canonical URLs of the substituters actually admitted.
    pub async fn add_relay_substituters(&mut self, urls: &[String]) -> Vec<String> {
        let mut admitted = Vec::new();
        for url in urls {
            match admit_substituter(url).await {
                Ok(substituter) => {
                    let canonical = substituter.url();
                    self.relay.insert(canonical.clone(), substituter);
                    admitted.push(canonical);
                }
                Err(err) => {
                    tracing::warn!(
                        substituter = %url,
                        error = %format!("{err:#}"),
                        "rejecting relay substituter from the archive manifest; paths only it \
                         could provide will be unavailable"
                    );
                }
            }
        }
        admitted
    }

    /// The admitted relay substituter with this canonical URL, if any.
    pub fn relay_substituter(&self, url: &str) -> Option<&Substituter> {
        self.relay.get(url)
    }

    /// Canonical URLs of every admitted relay substituter (sorted).
    pub fn relay_urls(&self) -> Vec<String> {
        let mut urls: Vec<String> = self.relay.keys().cloned().collect();
        urls.sort();
        urls
    }
}

/// Production [`SupplyTransport`] over the gateway SSH transport: a build
/// pool for probes and uploads, and an optional prefetch pool (dialed with
/// the prefetch tenant's key) for prefetch builds. Every operation acquires
/// a fresh daemon channel and abandons it after any error, so a failed op
/// can never leave a half-consumed wire position for the next one.
pub struct PoolSupplyTransport {
    /// Build-tenant pool: validity probes and client uploads.
    build: Arc<GatewayPool>,
    /// Prefetch-tenant pool; `None` when the supply policy has no prefetch arm.
    prefetch: Option<Arc<GatewayPool>>,
    /// Deadline for probes and batch uploads (streamed uploads use
    /// [`LARGE_UPLOAD_TIMEOUT`]).
    op_timeout: Duration,
    /// Paths per QueryValidPaths probe call.
    probe_chunk: usize,
    /// Concurrently held probe channels.
    probe_concurrency: usize,
}

impl PoolSupplyTransport {
    /// Build the transport from the campaign knobs and the dialed pools.
    pub fn new(build: Arc<GatewayPool>, prefetch: Option<Arc<GatewayPool>>, knobs: &Knobs) -> Self {
        Self {
            build,
            prefetch,
            op_timeout: Duration::from_secs(knobs.op_timeout_secs.max(1)),
            probe_chunk: knobs.probe_chunk.max(1),
            probe_concurrency: knobs.probe_concurrency.max(1),
        }
    }
}

/// Convert a prepared entry into the wire store entry the daemon ops take.
fn store_entry(entry: PreparedEntry) -> StoreEntry {
    StoreEntry {
        store_path: entry.store_path,
        info: entry.info,
        nar: entry.nar,
    }
}

#[async_trait]
impl SupplyTransport for PoolSupplyTransport {
    async fn query_valid(&self, paths: &[String]) -> anyhow::Result<BTreeSet<String>> {
        if paths.is_empty() {
            return Ok(BTreeSet::new());
        }
        let chunks: Vec<&[String]> = paths.chunks(self.probe_chunk).collect();
        // Open the probe channels once and hold them for the whole probe;
        // failing to open even one is a systemic error (the gateway is not
        // reachable at all), fewer than wanted just reduces concurrency.
        let wanted = self.probe_concurrency.clamp(1, chunks.len());
        let mut channels: Vec<DaemonChannel> = Vec::with_capacity(wanted);
        let mut last_error: Option<anyhow::Error> = None;
        for _ in 0..wanted {
            match self.build.open_channel().await {
                Ok(channel) => channels.push(channel),
                Err(err) => last_error = Some(err),
            }
        }
        if channels.is_empty() {
            let error = last_error.unwrap_or_else(|| anyhow!("no daemon channel could be opened"));
            return Err(error.context(
                "the supply stage could not open any daemon channel for the target validity \
                 probe; is the gateway reachable?",
            ));
        }
        let worker_count = channels.len();
        let workers = channels.into_iter().enumerate().map(|(worker, channel)| {
            let worker_chunks: Vec<&[String]> = chunks
                .iter()
                .skip(worker)
                .step_by(worker_count)
                .copied()
                .collect();
            async move {
                let mut valid = BTreeSet::new();
                let mut failed_chunks = 0u64;
                let mut channel: Option<DaemonChannel> = Some(channel);
                for chunk in worker_chunks {
                    if channel.is_none() {
                        match self.build.open_channel().await {
                            Ok(fresh) => channel = Some(fresh),
                            Err(err) => {
                                failed_chunks += 1;
                                tracing::warn!(
                                    error = %format!("{err:#}"),
                                    paths = chunk.len(),
                                    "could not reopen a validity probe channel; treating \
                                     the chunk's paths as not present"
                                );
                                continue;
                            }
                        }
                    }
                    let open_channel = channel.as_mut().expect("channel was just ensured above");
                    match open_channel.query_valid_paths(chunk, self.op_timeout).await {
                        Ok(found) => valid.extend(found),
                        Err(err) => {
                            failed_chunks += 1;
                            tracing::warn!(
                                error = %err,
                                paths = chunk.len(),
                                "target validity probe chunk failed; treating its paths as \
                                 not present"
                            );
                            // After any error the wire position is
                            // unknown; abandon the channel and dial a
                            // fresh one for the next chunk.
                            if let Some(broken) = channel.take() {
                                broken.abandon();
                            }
                        }
                    }
                }
                (valid, failed_chunks)
            }
        });
        let outcomes = join_all(workers).await;
        let mut valid = BTreeSet::new();
        let mut failed_chunks = 0u64;
        for (worker_valid, worker_failed) in outcomes {
            valid.extend(worker_valid);
            failed_chunks += worker_failed;
        }
        if failed_chunks > 0 {
            tracing::warn!(
                failed_chunks,
                "some validity probe chunks failed; their paths are treated as missing (the \
                 stage may upload more than necessary)"
            );
        }
        Ok(valid)
    }

    async fn upload_batch(&self, entries: Vec<PreparedEntry>) -> Result<(), SupplyTransportError> {
        let mut channel = self
            .build
            .open_channel()
            .await
            .context("open a daemon channel for the batch upload")
            .map_err(SupplyTransportError::Other)?;
        let entries: Vec<StoreEntry> = entries.into_iter().map(store_entry).collect();
        match channel
            .add_multiple_to_store(entries, self.op_timeout)
            .await
        {
            Ok(()) => Ok(()),
            Err(TransportError::Refused(message)) => {
                channel.abandon();
                Err(SupplyTransportError::Refused(message))
            }
            Err(other) => {
                channel.abandon();
                Err(SupplyTransportError::Other(
                    anyhow::Error::new(other).context("batch upload failed"),
                ))
            }
        }
    }

    async fn upload_streamed(&self, entry: PreparedEntry) -> Result<(), SupplyTransportError> {
        let mut channel = self
            .build
            .open_channel()
            .await
            .context("open a daemon channel for the streamed upload")
            .map_err(SupplyTransportError::Other)?;
        match channel
            .add_to_store_nar(store_entry(entry), LARGE_UPLOAD_TIMEOUT)
            .await
        {
            Ok(()) => Ok(()),
            Err(TransportError::Refused(message)) => {
                channel.abandon();
                Err(SupplyTransportError::Refused(message))
            }
            Err(other) => {
                channel.abandon();
                Err(SupplyTransportError::Other(
                    anyhow::Error::new(other).context("streamed upload failed"),
                ))
            }
        }
    }

    async fn prefetch_build(
        &self,
        roots: &[String],
        timeout: Duration,
    ) -> anyhow::Result<Vec<PathOutcome>> {
        let pool = self.prefetch.as_ref().ok_or_else(|| {
            anyhow!(
                "the supply transport has no prefetch pool (the prefetch arm needs \
                 cluster.ssh_key_dir and the prefetch tenant's key)"
            )
        })?;
        let mut channel = pool
            .open_channel()
            .await
            .context("open a daemon channel for the prefetch build")?;
        let derived: Vec<String> = roots.iter().map(|drv| format!("{drv}!*")).collect();
        match channel.build_paths_with_results(&derived, timeout).await {
            Ok(results) => Ok(roots
                .iter()
                .zip(results)
                .map(|(drv, keyed)| PathOutcome {
                    drv_path: drv.clone(),
                    status: build_status_name(keyed.result.status).to_string(),
                    error_msg: keyed.result.error_msg,
                    start_time: keyed.result.start_time,
                    stop_time: keyed.result.stop_time,
                })
                .collect()),
            Err(err) => {
                channel.abandon();
                Err(anyhow::Error::new(err).context("prefetch build over the prefetch tenant"))
            }
        }
    }
}

/// Run-wide dial circuit breaker for the upload arms.
///
/// Transport-level failures are expected occasionally, but when every
/// consecutive attempt fails the gateway is gone, and without a breaker each
/// remaining sub-batch would burn another connect timeout on a doomed
/// attempt. Any success (including a clean refusal, which proves the gateway
/// answered) resets the count; once tripped it stays tripped for the rest of
/// the run and remaining work is marked failed without further transport
/// calls.
struct GatewayBreaker {
    /// Consecutive transport failures since the last success.
    consecutive_failures: AtomicUsize,
    /// Failure count at which the breaker trips.
    threshold: usize,
    /// Latched once the threshold is reached.
    tripped: AtomicBool,
}

impl GatewayBreaker {
    /// Breaker that trips after `threshold` consecutive transport failures.
    fn new(threshold: usize) -> Self {
        Self {
            consecutive_failures: AtomicUsize::new(0),
            threshold: threshold.max(1),
            tripped: AtomicBool::new(false),
        }
    }

    /// Whether the breaker has tripped (the gateway is considered gone).
    fn is_tripped(&self) -> bool {
        self.tripped.load(Ordering::Relaxed)
    }

    /// Record a reachable gateway: the consecutive-failure count starts over.
    fn record_success(&self) {
        self.consecutive_failures.store(0, Ordering::Relaxed);
    }

    /// Record a transport failure; trips (and warns, exactly once) when the
    /// threshold is reached.
    fn record_failure(&self) {
        let failures = self.consecutive_failures.fetch_add(1, Ordering::Relaxed) + 1;
        if failures >= self.threshold && !self.tripped.swap(true, Ordering::Relaxed) {
            tracing::warn!(
                consecutive_failures = failures,
                "supply upload circuit breaker tripped: the gateway looks unreachable; remaining \
                 uploads are marked failed without further transport calls"
            );
        }
    }
}

/// Running totals of one upload-arm invocation.
#[derive(Debug, Clone, Default)]
struct UploadTotals {
    /// Paths the daemon accepted.
    delivered: usize,
    /// Paths refused even after the fresh-channel retry.
    refused: usize,
    /// Paths that failed (transport, materialization, breaker).
    failed: usize,
    /// Uncompressed NAR bytes of the delivered paths.
    uploaded_bytes: u64,
}

/// Shared, read-only inputs for one upload-arm invocation.
struct UploadEnv<'a> {
    /// The daemon operations seam.
    transport: &'a dyn SupplyTransport,
    /// The open archive (payload source for embedded paths); `None` degrades
    /// embedded payloads to failures (unit tests, archive-less calls).
    archive: Option<&'a Arc<ReplayArchive>>,
    /// Ladder context: relay substituters, validity knowledge.
    ctx: &'a SupplyContext,
    /// Campaign state dir for supply.jsonl appends.
    state: &'a StateDir,
    /// Cross-request upload claims.
    claims: &'a UploadClaims,
    /// Run-wide circuit breaker.
    breaker: &'a GatewayBreaker,
    /// Deadline for payload materialization and batch uploads.
    op_timeout: Duration,
    /// How long to wait on another request's claim before taking it over.
    claim_wait: Duration,
    /// Shared totals.
    totals: &'a std::sync::Mutex<UploadTotals>,
    /// Planned paths whose upload failed — their dependents are skipped.
    failed_paths: &'a std::sync::Mutex<BTreeSet<String>>,
    /// Upload sub-batch ordinal source (recorded as `SupplyEntry::batch_id`).
    sub_batch_ids: &'a AtomicU64,
}

/// One unit of upload work inside a topological level.
enum UploadWork<'a> {
    /// Stream this item individually (at or above the large-NAR threshold).
    Stream(&'a UploadItem),
    /// Send these items as one AddMultipleToStore batch.
    Batch(Vec<&'a UploadItem>),
}

/// The supply source recorded for an upload item, derived from its payload.
fn entry_source(item: &UploadItem) -> &'static str {
    match &item.payload {
        UploadPayload::DrvText(_) | UploadPayload::ArchivePath => SUPPLY_SOURCE_EMBEDDED,
        UploadPayload::Relay { .. } => SUPPLY_SOURCE_RELAY,
    }
}

/// Append one supply.jsonl line for an upload item and fold it into the
/// shared totals (refused/failed paths also poison their dependents).
fn record_settlement(
    env: &UploadEnv<'_>,
    item: &UploadItem,
    mechanism: &'static str,
    outcome: &'static str,
    detail: Option<String>,
    batch_id: u64,
    bytes: Option<u64>,
) -> Result<()> {
    env.state.append_jsonl(
        StateFile::Supply,
        &SupplyEntry {
            path: item.store_path.clone(),
            source: entry_source(item).to_string(),
            mechanism: mechanism.to_string(),
            outcome: outcome.to_string(),
            detail,
            batch_id: Some(batch_id),
            bytes,
            observed_at: now_rfc3339(),
        },
    )?;
    let mut totals = env
        .totals
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    if outcome == SUPPLY_OUTCOME_DELIVERED {
        totals.delivered += 1;
        totals.uploaded_bytes += bytes.unwrap_or(0);
    } else {
        if outcome == SUPPLY_OUTCOME_REFUSED {
            totals.refused += 1;
        } else {
            totals.failed += 1;
        }
        env.failed_paths
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .insert(item.store_path.clone());
    }
    Ok(())
}

/// What the cross-request claim table decided for one planned upload.
enum ClaimDecision {
    /// This invocation uploads the path.
    Upload,
    /// The path landed elsewhere; treat it as valid and record nothing.
    SkipLanded,
    /// Another request still holds the claim; proceed without the path.
    SkipHeld,
}

/// Claim `path` for upload, waiting (bounded) for another holder's claim and
/// re-claiming exactly once if that claim is released without landing.
async fn claim_for_upload(env: &UploadEnv<'_>, path: &str) -> ClaimDecision {
    match env.claims.claim(path) {
        ClaimOutcome::Won => ClaimDecision::Upload,
        ClaimOutcome::AlreadyDone => ClaimDecision::SkipLanded,
        ClaimOutcome::MustWait => {
            if env.claims.wait(path, env.claim_wait).await {
                return ClaimDecision::SkipLanded;
            }
            match env.claims.claim(path) {
                ClaimOutcome::Won => ClaimDecision::Upload,
                ClaimOutcome::AlreadyDone => ClaimDecision::SkipLanded,
                ClaimOutcome::MustWait => ClaimDecision::SkipHeld,
            }
        }
    }
}

/// First reference of `item` whose own planned upload already failed, if any.
fn failed_reference_of(env: &UploadEnv<'_>, item: &UploadItem) -> Option<String> {
    let failed = env
        .failed_paths
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    item.info
        .references
        .iter()
        .find(|reference| reference.as_str() != item.store_path && failed.contains(*reference))
        .cloned()
}

/// Produce the in-memory NAR bytes for one batch item just before sending
/// it, verifying the length against the path-info it will be sent with (a
/// mismatch would desync the upload stream and read as a daemon refusal).
/// The error string is the recorded failure detail.
async fn materialize_batch_payload(
    env: &UploadEnv<'_>,
    item: &UploadItem,
) -> std::result::Result<Vec<u8>, String> {
    let nar = match &item.payload {
        UploadPayload::DrvText(nar) => nar.clone(),
        UploadPayload::ArchivePath => {
            let Some(archive) = env.archive else {
                return Err("no replay archive is available to dump the embedded path".to_string());
            };
            let archive = Arc::clone(archive);
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
            let Some(substituter) = env.ctx.relay_substituter(substituter_url) else {
                return Err(format!(
                    "no admitted relay substituter matches {substituter_url}"
                ));
            };
            match substituter.fetch_nar(narinfo).await {
                Ok(nar) => nar,
                Err(err) => return Err(format!("relay fetch failed: {err:#}")),
            }
        }
    };
    if nar.len() as u64 != item.info.nar_size {
        return Err(format!(
            "materialized NAR is {} bytes but the path-info declares {}; not uploaded",
            nar.len(),
            item.info.nar_size
        ));
    }
    Ok(nar)
}

/// Produce the streamed payload for one large item: relayed paths stream
/// straight from their substituter, embedded paths and derivation texts are
/// materialized in memory (their bytes are already local).
async fn materialize_stream_payload(
    env: &UploadEnv<'_>,
    item: &UploadItem,
) -> std::result::Result<NarPayload, String> {
    match &item.payload {
        UploadPayload::Relay {
            substituter_url,
            narinfo,
        } => {
            let Some(substituter) = env.ctx.relay_substituter(substituter_url) else {
                return Err(format!(
                    "no admitted relay substituter matches {substituter_url}"
                ));
            };
            match substituter.fetch_nar_streaming(narinfo).await {
                Ok((len, reader)) => Ok(NarPayload::Reader { len, reader }),
                Err(err) => Err(format!("relay fetch failed: {err:#}")),
            }
        }
        UploadPayload::DrvText(_) | UploadPayload::ArchivePath => {
            materialize_batch_payload(env, item)
                .await
                .map(NarPayload::Bytes)
        }
    }
}

/// Stream one item to the target individually, with the single
/// refusal-retry on a fresh channel.
async fn upload_stream_one(env: &UploadEnv<'_>, item: &UploadItem) -> Result<()> {
    let batch_id = env.sub_batch_ids.fetch_add(1, Ordering::SeqCst);
    let mechanism = SUPPLY_MECHANISM_UPLOAD_STREAM;
    if env.breaker.is_tripped() {
        return record_settlement(
            env,
            item,
            mechanism,
            SUPPLY_OUTCOME_FAILED,
            Some(GATEWAY_UNREACHABLE.to_string()),
            batch_id,
            None,
        );
    }
    match claim_for_upload(env, &item.store_path).await {
        ClaimDecision::Upload => {}
        ClaimDecision::SkipLanded => {
            env.ctx
                .target_valid
                .write()
                .await
                .insert(item.store_path.clone());
            return Ok(());
        }
        ClaimDecision::SkipHeld => {
            tracing::debug!(
                path = %item.store_path,
                "skipping streamed upload; another request still holds its claim"
            );
            return Ok(());
        }
    }
    if let Some(reference) = failed_reference_of(env, item) {
        env.claims.release(&item.store_path);
        return record_settlement(
            env,
            item,
            mechanism,
            SUPPLY_OUTCOME_FAILED,
            Some(format!(
                "reference {reference} failed its earlier upload — skipped"
            )),
            batch_id,
            None,
        );
    }

    // First attempt, then exactly one retry when the daemon refused (the
    // refusal may be genuine or a transport error racing a refusal; one
    // retry on a fresh channel distinguishes a flake from a real rejection).
    let mut refused: Option<String> = None;
    for attempt in 0..2 {
        let payload = match materialize_stream_payload(env, item).await {
            Ok(payload) => payload,
            Err(detail) => {
                env.claims.release(&item.store_path);
                return record_settlement(
                    env,
                    item,
                    mechanism,
                    SUPPLY_OUTCOME_FAILED,
                    Some(detail),
                    batch_id,
                    None,
                );
            }
        };
        let entry = PreparedEntry {
            store_path: item.store_path.clone(),
            info: item.info.clone(),
            nar: payload,
        };
        match env.transport.upload_streamed(entry).await {
            Ok(()) => {
                env.breaker.record_success();
                env.claims.complete(&item.store_path);
                env.ctx
                    .target_valid
                    .write()
                    .await
                    .insert(item.store_path.clone());
                return record_settlement(
                    env,
                    item,
                    mechanism,
                    SUPPLY_OUTCOME_DELIVERED,
                    None,
                    batch_id,
                    Some(item.info.nar_size),
                );
            }
            Err(SupplyTransportError::Refused(message)) => {
                env.breaker.record_success();
                if attempt == 0 {
                    tracing::debug!(
                        path = %item.store_path,
                        refusal = %message,
                        "streamed upload refused; retrying once on a fresh channel"
                    );
                }
                refused = Some(message);
            }
            Err(SupplyTransportError::Other(err)) => {
                env.breaker.record_failure();
                env.claims.release(&item.store_path);
                return record_settlement(
                    env,
                    item,
                    mechanism,
                    SUPPLY_OUTCOME_FAILED,
                    Some(format!("streamed upload failed: {err:#}")),
                    batch_id,
                    None,
                );
            }
        }
    }
    env.claims.release(&item.store_path);
    record_settlement(
        env,
        item,
        mechanism,
        SUPPLY_OUTCOME_REFUSED,
        refused,
        batch_id,
        None,
    )
}

/// Result of one wire attempt of a batch upload.
enum BatchAttempt<'a> {
    /// The daemon accepted these entries.
    Sent(Vec<&'a UploadItem>),
    /// The daemon refused; `sendable` materialized fine and may be retried.
    Refused {
        /// Items whose payloads materialized and were part of the attempt.
        sendable: Vec<&'a UploadItem>,
        /// The daemon's refusal message.
        refusal: String,
    },
    /// Transport failure; `sendable` materialized fine but was not delivered.
    Failed {
        /// Items whose payloads materialized and were part of the attempt.
        sendable: Vec<&'a UploadItem>,
        /// The recorded failure detail.
        detail: String,
    },
    /// Nothing was left to send after materialization failures.
    Empty,
}

/// One wire attempt of a batch upload: materialize every payload (failures
/// drop the item individually), send the rest as one AddMultipleToStore.
async fn attempt_batch<'a>(
    env: &UploadEnv<'_>,
    items: &[&'a UploadItem],
    batch_id: u64,
) -> Result<BatchAttempt<'a>> {
    let mut sendable: Vec<&UploadItem> = Vec::with_capacity(items.len());
    let mut entries: Vec<PreparedEntry> = Vec::with_capacity(items.len());
    for &item in items {
        // The materialization deadline keeps a stalled cache connection from
        // wedging the level (batch NARs are below the large threshold, so
        // the generic op deadline is a fair ceiling).
        let materialized = match tokio::time::timeout(
            env.op_timeout,
            materialize_batch_payload(env, item),
        )
        .await
        {
            Ok(result) => result,
            Err(_elapsed) => Err(format!(
                "payload materialization timed out after {}s",
                env.op_timeout.as_secs()
            )),
        };
        match materialized {
            Ok(nar) => {
                sendable.push(item);
                entries.push(PreparedEntry {
                    store_path: item.store_path.clone(),
                    info: item.info.clone(),
                    nar: NarPayload::Bytes(nar),
                });
            }
            Err(detail) => {
                env.claims.release(&item.store_path);
                record_settlement(
                    env,
                    item,
                    SUPPLY_MECHANISM_UPLOAD_BATCH,
                    SUPPLY_OUTCOME_FAILED,
                    Some(detail),
                    batch_id,
                    None,
                )?;
            }
        }
    }
    if entries.is_empty() {
        return Ok(BatchAttempt::Empty);
    }
    match env.transport.upload_batch(entries).await {
        Ok(()) => {
            env.breaker.record_success();
            Ok(BatchAttempt::Sent(sendable))
        }
        Err(SupplyTransportError::Refused(refusal)) => {
            env.breaker.record_success();
            Ok(BatchAttempt::Refused { sendable, refusal })
        }
        Err(SupplyTransportError::Other(err)) => {
            env.breaker.record_failure();
            Ok(BatchAttempt::Failed {
                sendable,
                detail: format!("batch upload failed: {err:#}"),
            })
        }
    }
}

/// Bookkeeping after a batch landed: complete claims, extend the validity
/// set, and record one delivered entry per path.
async fn settle_batch_delivered(
    env: &UploadEnv<'_>,
    sent: &[&UploadItem],
    batch_id: u64,
) -> Result<()> {
    {
        let mut valid = env.ctx.target_valid.write().await;
        for item in sent {
            valid.insert(item.store_path.clone());
        }
    }
    for item in sent {
        env.claims.complete(&item.store_path);
        record_settlement(
            env,
            item,
            SUPPLY_MECHANISM_UPLOAD_BATCH,
            SUPPLY_OUTCOME_DELIVERED,
            None,
            batch_id,
            Some(item.info.nar_size),
        )?;
    }
    Ok(())
}

/// Record a non-delivered settlement for every item of a failed or refused
/// batch attempt, releasing their claims.
fn settle_batch_undelivered(
    env: &UploadEnv<'_>,
    items: &[&UploadItem],
    outcome: &'static str,
    detail: &str,
    batch_id: u64,
) -> Result<()> {
    for item in items {
        env.claims.release(&item.store_path);
        record_settlement(
            env,
            item,
            SUPPLY_MECHANISM_UPLOAD_BATCH,
            outcome,
            Some(detail.to_string()),
            batch_id,
            None,
        )?;
    }
    Ok(())
}

/// Upload one sub-batch: claims, failed-reference pre-skip, one wire
/// attempt, and exactly one retry on a fresh channel when the daemon
/// refused.
async fn upload_sub_batch(env: &UploadEnv<'_>, items: Vec<&UploadItem>) -> Result<()> {
    let batch_id = env.sub_batch_ids.fetch_add(1, Ordering::SeqCst);
    if env.breaker.is_tripped() {
        for item in &items {
            record_settlement(
                env,
                item,
                SUPPLY_MECHANISM_UPLOAD_BATCH,
                SUPPLY_OUTCOME_FAILED,
                Some(GATEWAY_UNREACHABLE.to_string()),
                batch_id,
                None,
            )?;
        }
        return Ok(());
    }
    // Cross-request claims: only paths this invocation wins are sent here.
    let mut kept: Vec<&UploadItem> = Vec::with_capacity(items.len());
    for item in items {
        match claim_for_upload(env, &item.store_path).await {
            ClaimDecision::Upload => kept.push(item),
            ClaimDecision::SkipLanded => {
                env.ctx
                    .target_valid
                    .write()
                    .await
                    .insert(item.store_path.clone());
            }
            ClaimDecision::SkipHeld => {
                tracing::debug!(
                    path = %item.store_path,
                    "skipping batch upload; another request still holds its claim"
                );
            }
        }
    }
    // An item whose reference already failed to upload would only be refused
    // by the daemon; skip it up front naming the real culprit. Levels run in
    // order, so this also covers transitive dependents in later levels.
    let mut to_send: Vec<&UploadItem> = Vec::with_capacity(kept.len());
    for item in kept {
        match failed_reference_of(env, item) {
            Some(reference) => {
                env.claims.release(&item.store_path);
                record_settlement(
                    env,
                    item,
                    SUPPLY_MECHANISM_UPLOAD_BATCH,
                    SUPPLY_OUTCOME_FAILED,
                    Some(format!(
                        "reference {reference} failed its earlier upload — skipped"
                    )),
                    batch_id,
                    None,
                )?;
            }
            None => to_send.push(item),
        }
    }
    if to_send.is_empty() {
        return Ok(());
    }

    match attempt_batch(env, &to_send, batch_id).await? {
        BatchAttempt::Sent(sent) => settle_batch_delivered(env, &sent, batch_id).await,
        BatchAttempt::Failed { sendable, detail } => {
            settle_batch_undelivered(env, &sendable, SUPPLY_OUTCOME_FAILED, &detail, batch_id)
        }
        BatchAttempt::Empty => Ok(()),
        BatchAttempt::Refused { sendable, refusal } => {
            tracing::debug!(
                paths = sendable.len(),
                refusal = %refusal,
                "batch upload refused; retrying once on a fresh channel with re-materialized \
                 payloads"
            );
            match attempt_batch(env, &sendable, batch_id).await? {
                BatchAttempt::Sent(sent) => settle_batch_delivered(env, &sent, batch_id).await,
                BatchAttempt::Refused { sendable, refusal } => settle_batch_undelivered(
                    env,
                    &sendable,
                    SUPPLY_OUTCOME_REFUSED,
                    &refusal,
                    batch_id,
                ),
                BatchAttempt::Failed { sendable, detail } => settle_batch_undelivered(
                    env,
                    &sendable,
                    SUPPLY_OUTCOME_FAILED,
                    &detail,
                    batch_id,
                ),
                BatchAttempt::Empty => Ok(()),
            }
        }
    }
}

/// Execute one upload plan: the planner's large items stream first, then the
/// batch level by level — level n+1 starts only after every upload of level
/// n settled, so references are always present before their referrers.
/// Within a level, batch items at or above the large-NAR threshold are
/// routed to individual streaming (the planner only size-routes relayed
/// paths; embedded payloads are routed by size here), and the remaining
/// sub-batches are spread over the upload workers.
async fn execute_plan(env: &UploadEnv<'_>, plan: &UploadPlan, knobs: &Knobs) -> Result<()> {
    let large_threshold = knobs.large_nar_threshold_mib.saturating_mul(1024 * 1024);
    for item in &plan.large {
        upload_stream_one(env, item).await?;
    }

    let target_valid = env.ctx.target_valid.read().await.clone();
    let levels = topo_levels(&plan.batch, &target_valid);
    for level in &levels {
        if level.is_empty() {
            continue;
        }
        let mut stream_items: Vec<&UploadItem> = Vec::new();
        let mut batch_items: Vec<&UploadItem> = Vec::new();
        for &index in level {
            let item = &plan.batch[index];
            if item.info.nar_size >= large_threshold {
                stream_items.push(item);
            } else {
                batch_items.push(item);
            }
        }
        let sub_batches = split_batches(
            &batch_items,
            knobs.upload_batch_max_mib.saturating_mul(1024 * 1024),
            knobs.upload_batch_max_entries,
        );
        let mut work: Vec<UploadWork<'_>> =
            stream_items.into_iter().map(UploadWork::Stream).collect();
        work.extend(sub_batches.into_iter().map(|indices| {
            UploadWork::Batch(
                indices
                    .into_iter()
                    .map(|index| batch_items[index])
                    .collect(),
            )
        }));
        if work.is_empty() {
            continue;
        }
        // Workers buy upload round-trip overlap, not bandwidth; more workers
        // than work units would only sit idle.
        let worker_count = knobs.upload_workers.max(1).min(work.len());
        let mut per_worker: Vec<Vec<UploadWork<'_>>> = Vec::with_capacity(worker_count);
        per_worker.resize_with(worker_count, Vec::new);
        for (index, unit) in work.into_iter().enumerate() {
            per_worker[index % worker_count].push(unit);
        }
        let workers = per_worker.into_iter().map(|units| async move {
            for unit in units {
                match unit {
                    UploadWork::Stream(item) => upload_stream_one(env, item).await?,
                    UploadWork::Batch(items) => upload_sub_batch(env, items).await?,
                }
            }
            Ok::<(), anyhow::Error>(())
        });
        for outcome in join_all(workers).await {
            outcome?;
        }
    }
    Ok(())
}

/// The bulk client-upload pass: push everything the upload plan contains in
/// reference-safe order before the execution clock starts, recording one
/// supply.jsonl entry per path as it settles. Per-path and per-batch
/// failures degrade (the affected paths fall back to the per-request
/// top-up); only state-dir write failures abort. Returns the upload half of
/// the stage report (delivered/refused/failed counts, bytes, throughput).
pub async fn prewarm_uploads(
    transport: &dyn SupplyTransport,
    archive: Option<&Arc<ReplayArchive>>,
    ctx: &SupplyContext,
    plan: &UploadPlan,
    knobs: &Knobs,
    state: &StateDir,
    claims: &UploadClaims,
) -> Result<SupplyStageReport> {
    let started = Instant::now();
    let breaker = GatewayBreaker::new(knobs.upload_workers.saturating_mul(2).max(6));
    let totals = std::sync::Mutex::new(UploadTotals::default());
    let failed_paths = std::sync::Mutex::new(BTreeSet::new());
    let sub_batch_ids = AtomicU64::new(0);
    let env = UploadEnv {
        transport,
        archive,
        ctx,
        state,
        claims,
        breaker: &breaker,
        op_timeout: Duration::from_secs(knobs.op_timeout_secs.max(1)),
        claim_wait: Duration::from_secs(knobs.claim_wait_mins.saturating_mul(60)),
        totals: &totals,
        failed_paths: &failed_paths,
        sub_batch_ids: &sub_batch_ids,
    };
    let planned = plan.large.len() + plan.batch.len();
    tracing::info!(
        planned,
        large = plan.large.len(),
        skipped_by_planner = plan.skipped.len(),
        "starting prewarm uploads"
    );
    execute_plan(&env, plan, knobs).await?;

    let totals = totals
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .clone();
    let upload_secs = started.elapsed().as_secs_f64();
    let upload_mib_per_s = (upload_secs > 0.0 && totals.uploaded_bytes > 0)
        .then(|| totals.uploaded_bytes as f64 / (1024.0 * 1024.0) / upload_secs);
    tracing::info!(
        delivered = totals.delivered,
        refused = totals.refused,
        failed = totals.failed,
        uploaded_mib = totals.uploaded_bytes / (1024 * 1024),
        elapsed_s = upload_secs,
        "prewarm uploads finished"
    );
    Ok(SupplyStageReport {
        delivered: totals.delivered,
        refused: totals.refused,
        failed: totals.failed,
        uploaded_bytes: totals.uploaded_bytes,
        upload_secs,
        upload_mib_per_s,
        ..SupplyStageReport::default()
    })
}

/// Map one prefetch root's build result to the recorded outcome and detail:
/// a successful substitution (or an already-valid path) is `delegated` with
/// detail `substituted`, a successful fallback build is `delegated` with
/// detail `built-fallback`, anything else is `failed` with the error message
/// (or the status name) as detail.
fn prefetch_disposition(result: &PathOutcome) -> (&'static str, Option<String>) {
    match build_status_from_name(&result.status) {
        Some(BuildStatus::Built) => (SUPPLY_OUTCOME_DELEGATED, Some("built-fallback".to_string())),
        Some(status) if status.is_success() => {
            (SUPPLY_OUTCOME_DELEGATED, Some("substituted".to_string()))
        }
        _ => (
            SUPPLY_OUTCOME_FAILED,
            Some(if result.error_msg.is_empty() {
                result.status.clone()
            } else {
                result.error_msg.clone()
            }),
        ),
    }
}

/// The scheduler-side prefetch arm: ask the target cluster (over the
/// prefetch tenant) to build/substitute the producing derivations of the
/// prefetch-planned paths, and record one supply.jsonl entry per path from
/// the per-root results. A failed chunk degrades to `failed` entries for its
/// paths; it never aborts the stage.
pub async fn prefetch_arm(
    transport: &dyn SupplyTransport,
    prefetch_roots: &BTreeMap<String, Vec<String>>,
    knobs: &Knobs,
    state: &StateDir,
    batch_seq: &AtomicU64,
) -> Result<PrefetchArmStats> {
    let mut stats = PrefetchArmStats::default();
    if prefetch_roots.is_empty() {
        return Ok(stats);
    }
    // Same child-deadline clamping as the submit loop: spec validation
    // rejects a non-positive batch_timeout_hours, but a Knobs value built
    // outside a loaded spec must never become a zero-second deadline.
    let timeout = Duration::from_secs((knobs.batch_timeout_hours * 3600.0).max(1.0) as u64);
    let chunk_size = knobs.batch_max_jobs.max(1);
    let roots: Vec<&String> = prefetch_roots.keys().collect();

    let mut record =
        |path: &str, outcome: &str, detail: Option<String>, batch_id: u64| -> Result<()> {
            state.append_jsonl(
                StateFile::Supply,
                &SupplyEntry {
                    path: path.to_string(),
                    source: SUPPLY_SOURCE_TARGET_SUBSTITUTER.to_string(),
                    mechanism: SUPPLY_MECHANISM_DELEGATE.to_string(),
                    outcome: outcome.to_string(),
                    detail,
                    batch_id: Some(batch_id),
                    bytes: None,
                    observed_at: now_rfc3339(),
                },
            )?;
            if outcome == SUPPLY_OUTCOME_DELEGATED {
                stats.delegated += 1;
            } else {
                stats.failed += 1;
            }
            Ok(())
        };

    for chunk in roots.chunks(chunk_size) {
        let batch_id = batch_seq.fetch_add(1, Ordering::SeqCst);
        let chunk_roots: Vec<String> = chunk.iter().map(|root| (*root).clone()).collect();
        tracing::info!(
            batch_id,
            roots = chunk_roots.len(),
            "prefetch arm: submitting prefetch build"
        );
        let outcomes = match transport.prefetch_build(&chunk_roots, timeout).await {
            Ok(outcomes) => outcomes,
            Err(err) => {
                let detail = format!("prefetch build failed: {err:#}");
                tracing::warn!(
                    batch_id,
                    error = %detail,
                    "prefetch chunk failed; recording its paths as failed"
                );
                for root in &chunk_roots {
                    for path in prefetch_roots.get(root).into_iter().flatten() {
                        record(path, SUPPLY_OUTCOME_FAILED, Some(detail.clone()), batch_id)?;
                    }
                }
                continue;
            }
        };
        let by_drv: HashMap<&str, &PathOutcome> = outcomes
            .iter()
            .map(|outcome| (outcome.drv_path.as_str(), outcome))
            .collect();
        for root in &chunk_roots {
            let (outcome, detail) = match by_drv.get(root.as_str()) {
                Some(result) => prefetch_disposition(result),
                None => (
                    SUPPLY_OUTCOME_FAILED,
                    Some("the prefetch build returned no result for this root".to_string()),
                ),
            };
            for path in prefetch_roots.get(root).into_iter().flatten() {
                record(path, outcome, detail.clone(), batch_id)?;
            }
        }
    }
    tracing::info!(
        delegated = stats.delegated,
        failed = stats.failed,
        "prefetch arm finished"
    );
    Ok(stats)
}

/// Per-submission gap top-up (the prewarm-miss fallback and the
/// inline-delivery path): probe validity for the roots' closure paths, plan
/// uploads for the gaps only (claims-deduplicated against concurrent
/// requests), and push them with the same machinery and vocabulary as the
/// prewarm pass.
pub async fn topup_for_roots(
    transport: &dyn SupplyTransport,
    archive: &Arc<ReplayArchive>,
    ctx: &SupplyContext,
    roots: &[String],
    knobs: &Knobs,
    state: &StateDir,
    claims: &UploadClaims,
) -> Result<()> {
    if roots.is_empty() {
        return Ok(());
    }
    // The closure walk reads and parses derivation texts — keep the
    // synchronous work off the async runtime.
    let closure = {
        let archive = Arc::clone(archive);
        let roots = roots.to_vec();
        tokio::task::spawn_blocking(move || walk_closure(&archive, &roots))
            .await
            .context("the top-up closure walk task panicked or was cancelled")??
    };

    // Probe only what is not already known valid; remember every answer.
    let known_valid = ctx.target_valid.read().await;
    let mut valid: BTreeSet<String> = closure
        .all_paths
        .iter()
        .filter(|path| known_valid.contains(*path))
        .cloned()
        .collect();
    let to_probe: Vec<String> = closure
        .all_paths
        .iter()
        .filter(|path| !known_valid.contains(*path))
        .cloned()
        .collect();
    drop(known_valid);
    if !to_probe.is_empty() {
        let probed = transport
            .query_valid(&to_probe)
            .await
            .context("probe target validity for the top-up roots")?;
        if !probed.is_empty() {
            ctx.target_valid
                .write()
                .await
                .extend(probed.iter().cloned());
            valid.extend(probed);
        }
    }

    // Resolve a source for every still-missing non-derivation path and plan
    // the uploads; claims are consulted by the upload arms themselves.
    let closure_drvs: BTreeSet<&str> = closure
        .topo
        .iter()
        .map(|node| node.drv_path.as_str())
        .collect();
    let input_srcs: BTreeSet<String> = closure
        .topo
        .iter()
        .flat_map(|node| node.input_srcs.iter().cloned())
        .collect();
    let mut sources: HashMap<String, PathSource> = HashMap::new();
    for path in &closure.all_paths {
        if closure_drvs.contains(path.as_str()) || valid.contains(path) {
            continue;
        }
        sources.insert(
            path.clone(),
            resolve_source(
                path,
                &ctx.workload_outputs,
                &ctx.target_coverage,
                &input_srcs,
                |candidate| archive.has_embedded(candidate),
                &ctx.relay_narinfos,
                ctx.dependencies,
            ),
        );
    }
    let plan = plan_uploads(
        &closure,
        &sources,
        &valid,
        archive,
        knobs.large_nar_threshold_mib.saturating_mul(1024 * 1024),
    )?;
    if !plan.skipped.is_empty() {
        tracing::debug!(
            roots = roots.len(),
            skipped = plan.skipped.len(),
            first = ?plan.skipped.first(),
            "top-up plan left paths unsupplied"
        );
    }
    if plan.large.is_empty() && plan.batch.is_empty() {
        return Ok(());
    }

    let breaker = GatewayBreaker::new(knobs.upload_workers.saturating_mul(2).max(6));
    let totals = std::sync::Mutex::new(UploadTotals::default());
    let failed_paths = std::sync::Mutex::new(BTreeSet::new());
    let sub_batch_ids = AtomicU64::new(0);
    let env = UploadEnv {
        transport,
        archive: Some(archive),
        ctx,
        state,
        claims,
        breaker: &breaker,
        op_timeout: Duration::from_secs(knobs.op_timeout_secs.max(1)),
        claim_wait: Duration::from_secs(knobs.claim_wait_mins.saturating_mul(60)),
        totals: &totals,
        failed_paths: &failed_paths,
        sub_batch_ids: &sub_batch_ids,
    };
    execute_plan(&env, &plan, knobs).await?;
    let totals = totals
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .clone();
    tracing::debug!(
        roots = roots.len(),
        delivered = totals.delivered,
        refused = totals.refused,
        failed = totals.failed,
        "top-up finished"
    );
    Ok(())
}

/// Append one bookkeeping-only supply.jsonl line (no batch, no bytes).
fn append_supply_entry(
    state: &StateDir,
    path: &str,
    source: &str,
    mechanism: &str,
    outcome: &str,
    detail: Option<String>,
) -> Result<()> {
    state.append_jsonl(
        StateFile::Supply,
        &SupplyEntry {
            path: path.to_string(),
            source: source.to_string(),
            mechanism: mechanism.to_string(),
            outcome: outcome.to_string(),
            detail,
            batch_id: None,
            bytes: None,
            observed_at: now_rfc3339(),
        },
    )
}

/// Probe `paths` against `substituters` in order (first hit wins), at most
/// `concurrency` paths in flight. Returns path → (canonical substituter URL,
/// narinfo) for the hits and folds per-cache probe-error counts into
/// `probe_errors` (warning once per cache). A probe error is never a miss:
/// the path simply falls through to the next substituter or ladder rung.
async fn probe_substituter_narinfos(
    substituters: &[&Substituter],
    paths: &[String],
    concurrency: usize,
    probe_errors: &mut BTreeMap<String, u64>,
) -> HashMap<String, (String, NarInfo)> {
    let mut hits = HashMap::new();
    if substituters.is_empty() || paths.is_empty() {
        return hits;
    }
    let mut probes = futures_util::stream::iter(paths.iter().map(|path| async move {
        let mut errored: Vec<String> = Vec::new();
        let hash_part = match rio_nix::store_path::StorePath::parse(path) {
            Ok(parsed) => parsed.hash_part(),
            Err(err) => {
                tracing::warn!(
                    path = %path,
                    error = %err,
                    "skipping the narinfo probe for an unparseable store path"
                );
                return (path.clone(), None, errored);
            }
        };
        for substituter in substituters {
            match substituter.narinfo(&hash_part).await {
                Ok(Some(narinfo)) => {
                    return (path.clone(), Some((substituter.url(), narinfo)), errored);
                }
                Ok(None) => {}
                Err(err) => {
                    tracing::debug!(
                        path = %path,
                        substituter = %substituter.url(),
                        error = %format!("{err:#}"),
                        "narinfo probe error; this substituter's coverage of the path is unknown"
                    );
                    errored.push(substituter.url());
                }
            }
        }
        (path.clone(), None, errored)
    }))
    .buffer_unordered(concurrency.max(1));
    while let Some((path, hit, errored)) = probes.next().await {
        for cache in errored {
            let count = probe_errors.entry(cache.clone()).or_insert(0);
            if *count == 0 {
                tracing::warn!(
                    substituter = %cache,
                    "narinfo probes against this substituter are erroring; probe errors are \
                     never treated as misses, the affected paths fall through the supply ladder"
                );
            }
            *count += 1;
        }
        if let Some((url, narinfo)) = hit {
            hits.insert(path, (url, narinfo));
        }
    }
    hits
}

/// The archive-backed half of the supply stage: walk the workload union
/// closure, extend target coverage with live substituter probes, resolve
/// every needed path through the source ladder, and deliver the resulting
/// upload plan (prewarm uploads, or an explicit deferral record under
/// inline delivery). Folds its outcomes into `report`.
async fn run_upload_ladder(
    state: &StateDir,
    transport: &dyn SupplyTransport,
    archive: &Arc<ReplayArchive>,
    inputs: &SupplyInputs,
    knobs: &Knobs,
    report: &mut SupplyStageReport,
) -> Result<()> {
    let roots: Vec<String> = inputs.workload_drvs.iter().cloned().collect();
    if roots.is_empty() {
        return Ok(());
    }
    // The closure walk reads and parses derivation texts (or adjacency
    // records) — keep the synchronous work off the async runtime.
    let closure = {
        let archive = Arc::clone(archive);
        let roots = roots.clone();
        tokio::task::spawn_blocking(move || walk_closure(&archive, &roots))
            .await
            .context("the supply closure walk task panicked or was cancelled")??
    };

    // Re-probe target validity for the whole closure: resume costs
    // re-probing, never correctness — nothing already present is re-sent.
    let to_probe: Vec<String> = closure.all_paths.iter().cloned().collect();
    let valid = transport
        .query_valid(&to_probe)
        .await
        .context("probe target validity for the workload union closure")?;

    let mut ctx = SupplyContext::new(inputs.dependencies);
    ctx.workload_outputs = inputs.workload_outputs.clone();
    ctx.target_coverage = inputs.target_coverage.clone();
    *ctx.target_valid.get_mut() = valid.clone();

    let closure_drvs: BTreeSet<&str> = closure
        .topo
        .iter()
        .map(|node| node.drv_path.as_str())
        .collect();
    let input_srcs: BTreeSet<String> = closure
        .topo
        .iter()
        .flat_map(|node| node.input_srcs.iter().cloned())
        .collect();
    // Paths the ladder still has to place: non-derivation closure members
    // not already valid on the target.
    let needed: Vec<String> = closure
        .all_paths
        .iter()
        .filter(|path| !closure_drvs.contains(path.as_str()) && !valid.contains(*path))
        .cloned()
        .collect();

    // Live coverage probes (target-substituter rung). Operator-provided
    // URLs are admitted as hard errors; the archive manifest's advisory
    // target list degrades like any other archive-sourced URL.
    if inputs.dependencies == SupplyDependencies::Substituters {
        let mut target_subs: Vec<Substituter> = Vec::new();
        if inputs.target_substituters.is_empty() {
            for url in &archive.manifest().substituters.target {
                match admit_substituter(url).await {
                    Ok(substituter) => target_subs.push(substituter),
                    Err(err) => tracing::warn!(
                        substituter = %url,
                        error = %format!("{err:#}"),
                        "rejecting target substituter from the archive manifest; coverage it \
                         could have provided is not probed"
                    ),
                }
            }
        } else {
            for url in &inputs.target_substituters {
                let substituter = admit_substituter(url).await.with_context(|| {
                    format!("supply.target_substituters entry {url:?} was rejected")
                })?;
                target_subs.push(substituter);
            }
        }
        let unprobed: Vec<String> = needed
            .iter()
            .filter(|path| {
                !ctx.workload_outputs.contains(*path) && !ctx.target_coverage.contains(*path)
            })
            .cloned()
            .collect();
        let target_refs: Vec<&Substituter> = target_subs.iter().collect();
        let hits = probe_substituter_narinfos(
            &target_refs,
            &unprobed,
            knobs.narinfo_concurrency,
            &mut report.probe_errors,
        )
        .await;
        ctx.target_coverage.extend(hits.into_keys());
    }

    // Relay substituters (archive-sourced) are admitted through the
    // public-cache guard and announced before any relay traffic; paths only
    // a rejected substituter could provide degrade to unavailable.
    let admitted = ctx.add_relay_substituters(&inputs.relay_substituters).await;
    if !admitted.is_empty() {
        tracing::info!(
            relay_substituters = ?admitted,
            "accepted relay substituters for the supply ladder"
        );
        let relay_candidates: Vec<String> = needed
            .iter()
            .filter(|path| {
                // Workload outputs are never supplied; embedded paths take
                // the archive rung before relay is ever consulted.
                if ctx.workload_outputs.contains(*path) || archive.has_embedded(path) {
                    return false;
                }
                // The target-substituter rung wins under the substituters
                // policy, so covered paths need no relay probe.
                if inputs.dependencies == SupplyDependencies::Substituters
                    && ctx.target_coverage.contains(*path)
                {
                    return false;
                }
                // Under the `none` policy only input sources may be relayed;
                // dependency outputs are withheld.
                inputs.dependencies != SupplyDependencies::None || input_srcs.contains(*path)
            })
            .cloned()
            .collect();
        let relay_hits = {
            let urls = ctx.relay_urls();
            let relay_refs: Vec<&Substituter> = urls
                .iter()
                .filter_map(|url| ctx.relay_substituter(url))
                .collect();
            probe_substituter_narinfos(
                &relay_refs,
                &relay_candidates,
                knobs.narinfo_concurrency,
                &mut report.probe_errors,
            )
            .await
        };
        ctx.relay_narinfos = relay_hits;
    }

    // Resolve every needed path through the source ladder. Paths the
    // resolution itself withholds or cannot place are recorded here, from
    // the resolution — the planner's skip list cannot distinguish a
    // policy-withheld dependency output from a path nothing has.
    let mut sources: HashMap<String, PathSource> = HashMap::new();
    for path in &needed {
        let source = resolve_source(
            path,
            &ctx.workload_outputs,
            &ctx.target_coverage,
            &input_srcs,
            |candidate| archive.has_embedded(candidate),
            &ctx.relay_narinfos,
            ctx.dependencies,
        );
        if source == (PathSource::NotSupplied { workload: false })
            // Prefetch-set paths already carry their own bookkeeping (the
            // upstream-coverage probe and the prefetch classification).
            && !inputs.prefetch_paths.contains_key(path)
        {
            let detail = if inputs.dependencies == SupplyDependencies::None
                && !input_srcs.contains(path)
            {
                "dependency output withheld by the supply policy (dependencies = \"none\")"
            } else {
                "no target substituter, archive member, or relay substituter can provide this path"
            };
            append_supply_entry(
                state,
                path,
                SUPPLY_SOURCE_NONE,
                SUPPLY_MECHANISM_NONE,
                SUPPLY_OUTCOME_UNAVAILABLE,
                Some(detail.to_string()),
            )?;
            report.unavailable += 1;
        }
        sources.insert(path.clone(), source);
    }

    let plan = plan_uploads(
        &closure,
        &sources,
        &valid,
        archive,
        knobs.large_nar_threshold_mib.saturating_mul(1024 * 1024),
    )?;

    match inputs.delivery {
        SupplyDelivery::Prewarm => {
            let claims = UploadClaims::new();
            let upload =
                prewarm_uploads(transport, Some(archive), &ctx, &plan, knobs, state, &claims)
                    .await?;
            report.delivered = upload.delivered;
            report.refused = upload.refused;
            report.failed += upload.failed;
            report.uploaded_bytes = upload.uploaded_bytes;
            report.upload_secs = upload.upload_secs;
            report.upload_mib_per_s = upload.upload_mib_per_s;
        }
        SupplyDelivery::Inline => {
            // Inline delivery defers planned uploads to the per-submission
            // top-up; the deferral is recorded so the report shows what was
            // deliberately not delivered before execution.
            for item in plan.large.iter().chain(plan.batch.iter()) {
                append_supply_entry(
                    state,
                    &item.store_path,
                    entry_source(item),
                    SUPPLY_MECHANISM_NONE,
                    SUPPLY_OUTCOME_UNAVAILABLE,
                    Some("deferred to inline top-up".to_string()),
                )?;
                report.unavailable += 1;
            }
        }
    }
    Ok(())
}

/// Run the whole supply stage: classify the prefetch set, delegate covered
/// paths to the target cluster via the prefetch arm, drive the upload ladder
/// over the workload union closure when an archive is available, and gate
/// execution on the prefetch shortfall.
///
/// Resume costs re-probing, never correctness: prior `supply.jsonl` content
/// is reporting-only and never read for decisions — validity is re-probed on
/// every run, already-valid paths are recorded `already-present`, and a
/// re-run after a crash re-converges without re-sending anything.
///
/// When the planned-but-missing prefetch fraction exceeds
/// `knobs.prefetch_shortfall_pause_pct`, the campaign PAUSE file is written
/// before returning; with `wait_for_resume` the call blocks (polling every
/// `cluster_status_poll_secs`) until an operator removes the file, so the
/// execution clock never starts on a silently under-supplied campaign.
pub async fn run_supply_stage(
    state: Arc<StateDir>,
    transport: Arc<dyn SupplyTransport>,
    inputs: SupplyInputs,
    knobs: &Knobs,
    batch_seq: Arc<AtomicU64>,
    wait_for_resume: bool,
) -> Result<SupplyStageReport> {
    let mut report = SupplyStageReport::default();

    // ── Prefetch classification ─────────────────────────────────────────
    let prefetch_candidates: Vec<String> = inputs
        .prefetch_paths
        .keys()
        .filter(|path| !inputs.prior_valid.contains(*path))
        .cloned()
        .collect();
    let probe_valid = transport
        .query_valid(&prefetch_candidates)
        .await
        .context("probe target validity for the prefetch set")?;

    let mut prefetch_roots: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for (path, producer) in &inputs.prefetch_paths {
        if inputs.prior_valid.contains(path) || probe_valid.contains(path) {
            append_supply_entry(
                &state,
                path,
                SUPPLY_SOURCE_NONE,
                SUPPLY_MECHANISM_NONE,
                SUPPLY_OUTCOME_ALREADY_PRESENT,
                None,
            )?;
            report.already_present += 1;
            continue;
        }
        let covered = inputs.target_coverage.contains(path);
        match (covered, producer) {
            // Not covered upstream: a prefetch submission could not
            // substitute it. With a producer the upload ladder below may
            // still deliver it; without one nothing can.
            (false, Some(_)) => {}
            (false, None) => {
                append_supply_entry(
                    &state,
                    path,
                    SUPPLY_SOURCE_TARGET_SUBSTITUTER,
                    SUPPLY_MECHANISM_NONE,
                    SUPPLY_OUTCOME_UNAVAILABLE,
                    Some(
                        "not covered by a target substituter and no static producing \
                         derivation to prefetch"
                            .to_string(),
                    ),
                )?;
                report.unavailable += 1;
            }
            // Covered but with no static producing derivation
            // (content-addressed / floating outputs): there is no drv to
            // submit for it, so it cannot be prefetched.
            (true, None) => {
                append_supply_entry(
                    &state,
                    path,
                    SUPPLY_SOURCE_TARGET_SUBSTITUTER,
                    SUPPLY_MECHANISM_NONE,
                    SUPPLY_OUTCOME_UNAVAILABLE,
                    Some("no static producing derivation to prefetch".to_string()),
                )?;
                report.unavailable += 1;
            }
            (true, Some(drv)) => {
                prefetch_roots
                    .entry(drv.clone())
                    .or_default()
                    .push(path.clone());
            }
        }
    }
    report.planned_prefetch = prefetch_roots.values().map(Vec::len).sum();

    // ── Prefetch arm (delegate covered paths to the target cluster) ─────
    let stats = prefetch_arm(
        transport.as_ref(),
        &prefetch_roots,
        knobs,
        &state,
        &batch_seq,
    )
    .await?;
    report.delegated = stats.delegated;
    report.prefetch_missing = stats.failed;
    report.failed += stats.failed;

    // ── Upload ladder over the workload union closure ───────────────────
    if let Some(archive) = inputs.archive.clone() {
        run_upload_ladder(
            &state,
            transport.as_ref(),
            &archive,
            &inputs,
            knobs,
            &mut report,
        )
        .await?;
    }

    // ── Prefetch shortfall gate ─────────────────────────────────────────
    if report.planned_prefetch > 0 {
        let shortfall_pct = report.prefetch_missing as f64 / report.planned_prefetch as f64 * 100.0;
        report.shortfall_pct = Some(shortfall_pct);
        if shortfall_pct > knobs.prefetch_shortfall_pause_pct {
            let pause = state.path("PAUSE");
            std::fs::write(&pause, b"prefetch shortfall\n")
                .with_context(|| format!("write {}", pause.display()))?;
            tracing::error!(
                prefetch_missing = report.prefetch_missing,
                planned_prefetch = report.planned_prefetch,
                shortfall_pct,
                threshold_pct = knobs.prefetch_shortfall_pause_pct,
                "prefetch shortfall above the pause threshold; the campaign is paused before \
                 execution — remove the PAUSE file to accept the shortfall and resume, or abort"
            );
            if wait_for_resume {
                let poll = Duration::from_secs(knobs.cluster_status_poll_secs.max(1));
                while state.path("PAUSE").exists() {
                    tokio::time::sleep(poll).await;
                }
                tracing::info!("PAUSE file removed; resuming after the prefetch shortfall");
            }
        }
    }

    Ok(report)
}

/// Re-derive the per-path outcome counts of a [`SupplyStageReport`] from the
/// supply journal, keeping only the latest record per path.
///
/// The journal legitimately carries more than one row for a path — the
/// upstream-coverage probe records `unavailable` before the supply stage
/// runs, and a path one arm could not provide can be delivered by a later
/// arm or a top-up — so counting raw rows would double-count. The last row
/// per path is its settled disposition. Throughput, prefetch-shortfall, and
/// probe-error figures are not per-path counts and stay as the stage
/// reported them. An empty journal leaves the report untouched.
pub fn refresh_outcome_counts(report: &mut SupplyStageReport, entries: &[SupplyEntry]) {
    if entries.is_empty() {
        return;
    }
    let mut latest: BTreeMap<&str, &str> = BTreeMap::new();
    for entry in entries {
        latest.insert(entry.path.as_str(), entry.outcome.as_str());
    }
    let count = |outcome: &str| latest.values().filter(|got| **got == outcome).count();
    report.delivered = count(SUPPLY_OUTCOME_DELIVERED);
    report.delegated = count(SUPPLY_OUTCOME_DELEGATED);
    report.already_present = count(SUPPLY_OUTCOME_ALREADY_PRESENT);
    report.refused = count(SUPPLY_OUTCOME_REFUSED);
    report.unavailable = count(SUPPLY_OUTCOME_UNAVAILABLE);
    report.failed = count(SUPPLY_OUTCOME_FAILED);
}

/// Resident-set size of this process in MiB, read from `/proc/self/status`
/// (`VmRSS`); `None` when the file or the field is unavailable.
pub fn current_rss_mib() -> Option<u64> {
    proc_status_mib("VmRSS:")
}

/// Peak resident-set size (high-water mark, `VmHWM`) of this process in MiB;
/// `None` when the file or the field is unavailable.
pub fn peak_rss_mib() -> Option<u64> {
    proc_status_mib("VmHWM:")
}

/// Read one `kB`-denominated field of `/proc/self/status` and convert it to
/// MiB.
fn proc_status_mib(field: &str) -> Option<u64> {
    let status = std::fs::read_to_string("/proc/self/status").ok()?;
    for line in status.lines() {
        if let Some(rest) = line.strip_prefix(field) {
            let kib: u64 = rest.trim().trim_end_matches("kB").trim().parse().ok()?;
            return Some(kib / 1024);
        }
    }
    None
}

/// Scripted [`SupplyTransport`] for stage-level tests: records every call,
/// answers validity from an in-memory set, and lets tests script refusals,
/// hard transport failures, and per-root prefetch results.
#[cfg(test)]
pub(crate) mod test_support {
    use std::sync::Mutex;

    use super::*;

    /// See the module-level doc: a fully scripted, in-memory transport.
    #[derive(Default)]
    pub struct FakeSupplyTransport {
        /// Paths "valid on the target": probes answer from it, successful
        /// uploads add to it.
        pub valid: Mutex<BTreeSet<String>>,
        /// Store paths of every accepted batch upload, one inner vec per call.
        pub uploaded_batches: Mutex<Vec<Vec<String>>>,
        /// Store paths of every accepted streamed upload, in call order.
        pub uploaded_streamed: Mutex<Vec<String>>,
        /// Path → number of times to refuse it before accepting.
        pub refusals: Mutex<HashMap<String, u32>>,
        /// Root drv → BuildStatus name returned by `prefetch_build`
        /// (unscripted roots report `Substituted`).
        pub prefetch_results: Mutex<HashMap<String, String>>,
        /// When set, every upload fails with [`SupplyTransportError::Other`]
        /// (the shape of a channel-open failure).
        pub fail_uploads: AtomicBool,
        /// Total upload calls (batch + streamed), accepted or not.
        pub upload_calls: AtomicUsize,
        /// Per-path upload attempts (batch + streamed), accepted or not.
        pub attempts: Mutex<HashMap<String, u32>>,
    }

    impl FakeSupplyTransport {
        /// Count one upload attempt for every path of the call.
        fn note_attempts(&self, paths: &[String]) {
            let mut attempts = self.attempts.lock().unwrap();
            for path in paths {
                *attempts.entry(path.clone()).or_insert(0) += 1;
            }
        }

        /// Consume one scripted refusal if any path of the call still has one.
        fn refusal_for(&self, paths: &[String]) -> Option<String> {
            let mut refusals = self.refusals.lock().unwrap();
            for path in paths {
                if let Some(left) = refusals.get_mut(path)
                    && *left > 0
                {
                    *left -= 1;
                    return Some(format!("scripted refusal for {path}"));
                }
            }
            None
        }

        /// Common upload bookkeeping; `Ok(())` means the upload is accepted.
        fn accept_upload(&self, paths: &[String]) -> Result<(), SupplyTransportError> {
            self.upload_calls.fetch_add(1, Ordering::SeqCst);
            self.note_attempts(paths);
            if self.fail_uploads.load(Ordering::SeqCst) {
                return Err(SupplyTransportError::Other(anyhow!(
                    "scripted transport failure"
                )));
            }
            if let Some(refusal) = self.refusal_for(paths) {
                return Err(SupplyTransportError::Refused(refusal));
            }
            self.valid.lock().unwrap().extend(paths.iter().cloned());
            Ok(())
        }
    }

    #[async_trait]
    impl SupplyTransport for FakeSupplyTransport {
        async fn query_valid(&self, paths: &[String]) -> anyhow::Result<BTreeSet<String>> {
            let valid = self.valid.lock().unwrap();
            Ok(paths
                .iter()
                .filter(|path| valid.contains(*path))
                .cloned()
                .collect())
        }

        async fn upload_batch(
            &self,
            entries: Vec<PreparedEntry>,
        ) -> Result<(), SupplyTransportError> {
            let paths: Vec<String> = entries.iter().map(|e| e.store_path.clone()).collect();
            self.accept_upload(&paths)?;
            self.uploaded_batches.lock().unwrap().push(paths);
            Ok(())
        }

        async fn upload_streamed(&self, entry: PreparedEntry) -> Result<(), SupplyTransportError> {
            let paths = vec![entry.store_path.clone()];
            self.accept_upload(&paths)?;
            self.uploaded_streamed
                .lock()
                .unwrap()
                .push(entry.store_path);
            Ok(())
        }

        async fn prefetch_build(
            &self,
            roots: &[String],
            _timeout: Duration,
        ) -> anyhow::Result<Vec<PathOutcome>> {
            let scripted = self.prefetch_results.lock().unwrap();
            Ok(roots
                .iter()
                .map(|drv| {
                    let status = scripted
                        .get(drv)
                        .cloned()
                        .unwrap_or_else(|| build_status_name(BuildStatus::Substituted).to_string());
                    let failed = build_status_from_name(&status)
                        .map(|s| !s.is_success())
                        .unwrap_or(true);
                    PathOutcome {
                        drv_path: drv.clone(),
                        status: status.clone(),
                        error_msg: if failed {
                            format!("scripted prefetch failure ({status})")
                        } else {
                            String::new()
                        },
                        start_time: 0,
                        stop_time: 0,
                    }
                })
                .collect())
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::AtomicU64;

    use super::test_support::FakeSupplyTransport;
    use super::*;

    const PATH_A: &str = "/nix/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-supply-a";
    const PATH_B: &str = "/nix/store/bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb-supply-b";

    fn state() -> (tempfile::TempDir, StateDir) {
        let dir = tempfile::tempdir().unwrap();
        let state = StateDir::new(dir.path()).unwrap();
        (dir, state)
    }

    /// Hand-made batch item: the payload is its own NAR bytes, so the
    /// declared `nar_size` always matches what materialization produces.
    fn item(store_path: &str, nar: Vec<u8>, references: &[&str]) -> UploadItem {
        UploadItem {
            store_path: store_path.to_string(),
            info: ValidPathInfo {
                deriver: None,
                nar_hash: vec![0u8; 32],
                references: references.iter().map(|r| r.to_string()).collect(),
                registration_time: 0,
                nar_size: nar.len() as u64,
                ultimate: false,
                signatures: Vec::new(),
                content_address: None,
            },
            payload: UploadPayload::DrvText(nar),
        }
    }

    fn batch_plan(items: Vec<UploadItem>) -> UploadPlan {
        UploadPlan {
            large: Vec::new(),
            batch: items,
            skipped: Vec::new(),
        }
    }

    fn entries(state: &StateDir) -> Vec<SupplyEntry> {
        state.load_jsonl(StateFile::Supply).unwrap()
    }

    fn entry_for<'a>(entries: &'a [SupplyEntry], path: &str) -> &'a SupplyEntry {
        entries
            .iter()
            .find(|entry| entry.path == path)
            .unwrap_or_else(|| panic!("no supply entry for {path}: {entries:?}"))
    }

    #[tokio::test]
    async fn prewarm_uploads_in_reference_order_and_records_supply_entries() {
        let (_dir, state) = state();
        let fake = FakeSupplyTransport::default();
        let ctx = SupplyContext::new(SupplyDependencies::Substituters);
        let claims = UploadClaims::new();
        let plan = batch_plan(vec![
            item(PATH_A, vec![1u8; 100], &[]),
            item(PATH_B, vec![2u8; 50], &[PATH_A]),
        ]);

        let report = prewarm_uploads(&fake, None, &ctx, &plan, &Knobs::default(), &state, &claims)
            .await
            .unwrap();

        // B references A, so A's upload batch settles no later than B's.
        let entries = entries(&state);
        let entry_a = entry_for(&entries, PATH_A);
        let entry_b = entry_for(&entries, PATH_B);
        assert!(entry_a.batch_id.unwrap() <= entry_b.batch_id.unwrap());
        for entry in [entry_a, entry_b] {
            assert_eq!(entry.mechanism, SUPPLY_MECHANISM_UPLOAD_BATCH);
            assert_eq!(entry.outcome, SUPPLY_OUTCOME_DELIVERED);
            assert!(entry.bytes.unwrap() > 0);
        }
        assert_eq!(report.delivered, 2);
        assert!(report.uploaded_bytes > 0);
        // The fake observed A strictly before B (separate reference levels).
        let uploaded = fake.uploaded_batches.lock().unwrap().clone();
        assert_eq!(
            uploaded,
            vec![vec![PATH_A.to_string()], vec![PATH_B.to_string()]]
        );
    }

    #[tokio::test]
    async fn upload_refusal_retries_once_then_marks_refused() {
        let (_dir, state) = state();
        let fake = FakeSupplyTransport::default();
        fake.refusals.lock().unwrap().insert(PATH_A.to_string(), 2);
        let ctx = SupplyContext::new(SupplyDependencies::Substituters);
        let claims = UploadClaims::new();
        let plan = batch_plan(vec![item(PATH_A, vec![1u8; 64], &[])]);

        let report = prewarm_uploads(&fake, None, &ctx, &plan, &Knobs::default(), &state, &claims)
            .await
            .unwrap();

        // Exactly two attempts: the original send plus one retry on a fresh
        // channel, then the path is marked refused.
        assert_eq!(fake.attempts.lock().unwrap()[PATH_A], 2);
        let entries = entries(&state);
        assert_eq!(entry_for(&entries, PATH_A).outcome, SUPPLY_OUTCOME_REFUSED);
        assert_eq!(report.refused, 1);
        assert_eq!(report.delivered, 0);
        assert!(fake.uploaded_batches.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn large_items_stream_individually() {
        let (_dir, state) = state();
        let fake = FakeSupplyTransport::default();
        let ctx = SupplyContext::new(SupplyDependencies::Substituters);
        let claims = UploadClaims::new();
        let knobs = Knobs {
            large_nar_threshold_mib: 1,
            ..Knobs::default()
        };
        // One small item and one item at twice the large-NAR threshold; both
        // sit in the planner's batch, so the size routing under test is the
        // execution side's.
        let large_nar = vec![3u8; 2 * 1024 * 1024];
        let plan = batch_plan(vec![
            item(PATH_A, vec![1u8; 64], &[]),
            item(PATH_B, large_nar, &[]),
        ]);

        prewarm_uploads(&fake, None, &ctx, &plan, &knobs, &state, &claims)
            .await
            .unwrap();

        assert_eq!(
            fake.uploaded_streamed.lock().unwrap().clone(),
            vec![PATH_B.to_string()]
        );
        assert_eq!(
            fake.uploaded_batches.lock().unwrap().clone(),
            vec![vec![PATH_A.to_string()]]
        );
        let entries = entries(&state);
        assert_eq!(
            entry_for(&entries, PATH_B).mechanism,
            SUPPLY_MECHANISM_UPLOAD_STREAM
        );
        assert_eq!(
            entry_for(&entries, PATH_A).mechanism,
            SUPPLY_MECHANISM_UPLOAD_BATCH
        );
        for path in [PATH_A, PATH_B] {
            assert_eq!(entry_for(&entries, path).outcome, SUPPLY_OUTCOME_DELIVERED);
        }
    }

    #[tokio::test]
    async fn circuit_breaker_latches_after_consecutive_open_failures() {
        let (_dir, state) = state();
        let fake = FakeSupplyTransport::default();
        fake.fail_uploads.store(true, Ordering::SeqCst);
        let ctx = SupplyContext::new(SupplyDependencies::Substituters);
        let claims = UploadClaims::new();
        // One worker and one entry per sub-batch make the failure sequence
        // strictly serial: the breaker threshold is max(2 × 1, 6) = 6.
        let knobs = Knobs {
            upload_workers: 1,
            upload_batch_max_entries: 1,
            ..Knobs::default()
        };
        let total_items = 12usize;
        let items: Vec<UploadItem> = (0..total_items)
            .map(|index| {
                item(
                    &format!("/nix/store/{:032}-breaker-{index}", index),
                    vec![1u8; 8],
                    &[],
                )
            })
            .collect();
        let paths: Vec<String> = items.iter().map(|i| i.store_path.clone()).collect();
        let plan = batch_plan(items);

        let report = prewarm_uploads(&fake, None, &ctx, &plan, &knobs, &state, &claims)
            .await
            .unwrap();

        // Six consecutive channel-level failures latch the breaker; the
        // remaining six planned paths are recorded failed without any
        // further transport calls.
        assert_eq!(fake.upload_calls.load(Ordering::SeqCst), 6);
        assert_eq!(report.failed, total_items);
        assert_eq!(report.delivered, 0);
        let entries = entries(&state);
        let mut not_retried = 0usize;
        for path in &paths {
            let entry = entry_for(&entries, path);
            assert_eq!(entry.outcome, SUPPLY_OUTCOME_FAILED);
            if entry.detail.as_deref() == Some(GATEWAY_UNREACHABLE) {
                not_retried += 1;
            }
        }
        assert_eq!(not_retried, total_items - 6);
    }

    #[tokio::test]
    async fn prefetch_arm_maps_per_root_results_to_dispositions() {
        let (_dir, state) = state();
        let fake = FakeSupplyTransport::default();
        let drv_a = "/nix/store/cccccccccccccccccccccccccccccccc-a.drv";
        let drv_b = "/nix/store/dddddddddddddddddddddddddddddddd-b.drv";
        let drv_c = "/nix/store/eeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee-c.drv";
        let path_a = "/nix/store/ffffffffffffffffffffffffffffffff-out-a";
        let path_b = "/nix/store/gggggggggggggggggggggggggggggggg-out-b";
        let path_c = "/nix/store/hhhhhhhhhhhhhhhhhhhhhhhhhhhhhhhh-out-c";
        {
            let mut scripted = fake.prefetch_results.lock().unwrap();
            scripted.insert(
                drv_a.to_string(),
                build_status_name(BuildStatus::Substituted).to_string(),
            );
            scripted.insert(
                drv_b.to_string(),
                build_status_name(BuildStatus::Built).to_string(),
            );
            scripted.insert(
                drv_c.to_string(),
                build_status_name(BuildStatus::PermanentFailure).to_string(),
            );
        }
        let prefetch_roots: BTreeMap<String, Vec<String>> = [
            (drv_a.to_string(), vec![path_a.to_string()]),
            (drv_b.to_string(), vec![path_b.to_string()]),
            (drv_c.to_string(), vec![path_c.to_string()]),
        ]
        .into();
        let batch_seq = AtomicU64::new(7);

        let stats = prefetch_arm(
            &fake,
            &prefetch_roots,
            &Knobs::default(),
            &state,
            &batch_seq,
        )
        .await
        .unwrap();

        let entries = entries(&state);
        let entry_a = entry_for(&entries, path_a);
        let entry_b = entry_for(&entries, path_b);
        let entry_c = entry_for(&entries, path_c);
        assert_eq!(entry_a.outcome, SUPPLY_OUTCOME_DELEGATED);
        assert_eq!(entry_a.detail.as_deref(), Some("substituted"));
        assert_eq!(entry_b.outcome, SUPPLY_OUTCOME_DELEGATED);
        assert_eq!(entry_b.detail.as_deref(), Some("built-fallback"));
        assert_eq!(entry_c.outcome, SUPPLY_OUTCOME_FAILED);
        for entry in [entry_a, entry_b, entry_c] {
            assert_eq!(entry.mechanism, SUPPLY_MECHANISM_DELEGATE);
            assert_eq!(entry.source, SUPPLY_SOURCE_TARGET_SUBSTITUTER);
            assert_eq!(entry.batch_id, Some(7));
        }
        assert_eq!(stats.delegated, 2);
        assert_eq!(stats.failed, 1);
    }

    #[test]
    fn gateway_breaker_trips_after_consecutive_failures() {
        let breaker = GatewayBreaker::new(3);
        breaker.record_failure();
        breaker.record_failure();
        assert!(!breaker.is_tripped());

        // A success in between starts the count over.
        breaker.record_success();
        breaker.record_failure();
        breaker.record_failure();
        assert!(!breaker.is_tripped());

        // The third consecutive failure trips it, and it stays tripped (the
        // upload arms never dial again afterwards).
        breaker.record_failure();
        assert!(breaker.is_tripped());
        breaker.record_success();
        assert!(breaker.is_tripped());
    }

    #[tokio::test]
    async fn admit_substituter_guards_spec_and_archive_urls() {
        // An accepted form passes the guard and produces a usable client.
        // The accept case uses s3 because building an HTTPS client needs the
        // platform trust store, which the hermetic test sandbox lacks; the
        // https accept path is covered by the validator's own unit tests.
        let admitted = admit_substituter("s3://nix-cache-bucket/prefix?region=us-east-1")
            .await
            .unwrap();
        assert_eq!(admitted.url(), "s3://nix-cache-bucket/prefix/");
        // Plain HTTP and non-public hosts are rejected BEFORE parse — the
        // permissive `Substituter::parse` (which accepts both for offline
        // tests and dev flows) never sees them.
        for url in ["http://cache.nixos.org", "https://127.0.0.1:8080"] {
            let err = format!("{:#}", admit_substituter(url).await.unwrap_err());
            assert!(err.contains("supply substituter"), "{err}");
        }
    }

    #[test]
    fn rss_helpers_read_proc_self_status() {
        // /proc/self/status is always present on Linux (the only platform
        // the engine targets); both fields parse to a non-zero MiB value.
        assert!(current_rss_mib().unwrap() > 0);
        assert!(peak_rss_mib().unwrap() > 0);
    }

    #[test]
    fn journal_outcome_counts_dedupe_latest_per_path() {
        let entry = |path: &str, outcome: &str| SupplyEntry {
            path: path.into(),
            source: SUPPLY_SOURCE_TARGET_SUBSTITUTER.into(),
            mechanism: SUPPLY_MECHANISM_NONE.into(),
            outcome: outcome.into(),
            detail: None,
            batch_id: None,
            bytes: None,
            observed_at: now_rfc3339(),
        };
        const PATH_C: &str = "/nix/store/cccccccccccccccccccccccccccccccc-supply-c";
        let entries = vec![
            // The coverage probe found no upstream narinfo, then the supply
            // stage re-recorded the same path: one unavailable path, not two.
            entry(PATH_A, SUPPLY_OUTCOME_UNAVAILABLE),
            entry(PATH_A, SUPPLY_OUTCOME_UNAVAILABLE),
            // Failed by one arm, later delivered: only the settled
            // disposition counts.
            entry(PATH_B, SUPPLY_OUTCOME_FAILED),
            entry(PATH_B, SUPPLY_OUTCOME_DELIVERED),
            entry(PATH_C, SUPPLY_OUTCOME_DELEGATED),
        ];
        let mut report = SupplyStageReport {
            unavailable: 9,
            failed: 9,
            ..SupplyStageReport::default()
        };
        refresh_outcome_counts(&mut report, &entries);
        assert_eq!(report.unavailable, 1);
        assert_eq!(report.delivered, 1);
        assert_eq!(report.failed, 0);
        assert_eq!(report.delegated, 1);

        // An empty journal leaves the stage's own tallies untouched.
        let mut untouched = SupplyStageReport {
            delivered: 3,
            ..SupplyStageReport::default()
        };
        refresh_outcome_counts(&mut untouched, &[]);
        assert_eq!(untouched.delivered, 3);
    }

    #[test]
    fn tenant_key_path_joins_only_plain_file_names() {
        // The tenant name is joined onto the key directory as a path
        // component; traversal sequences, separators, and empty names must
        // never reach that join.
        assert_eq!(
            tenant_key_path(std::path::Path::new("/etc/rio/parity-ssh"), "parity-warm").unwrap(),
            std::path::PathBuf::from("/etc/rio/parity-ssh/parity-warm")
        );
        for bad in ["../evil", "a/b", "", "warm tenant"] {
            let err = tenant_key_path(std::path::Path::new("/keys"), bad)
                .unwrap_err()
                .to_string();
            assert!(
                err.contains("invalid tenant name") && err.contains(bad),
                "tenant {bad:?} must be rejected with an error naming it: {err}"
            );
        }
    }

    /// Supply inputs for the prefetch-shortfall tests: `paths` prefetch
    /// paths, every one covered by a target substituter and produced by its
    /// own drv (`drv-of(path)`), no archive (the stage degrades to the
    /// prefetch arm only).
    fn prefetch_only_inputs(paths: &[String]) -> SupplyInputs {
        SupplyInputs {
            workload_outputs: BTreeSet::new(),
            workload_drvs: BTreeSet::new(),
            prefetch_paths: paths
                .iter()
                .map(|path| (path.clone(), Some(producing_drv(path))))
                .collect(),
            prior_valid: BTreeSet::new(),
            target_coverage: paths.iter().cloned().collect(),
            archive: None,
            target_substituters: Vec::new(),
            relay_substituters: Vec::new(),
            dependencies: SupplyDependencies::Substituters,
            delivery: crate::run::spec::SupplyDelivery::Prewarm,
        }
    }

    /// Deterministic fake producing drv for one prefetch path.
    fn producing_drv(path: &str) -> String {
        let name = path.rsplit('-').next().unwrap_or("x");
        format!("/nix/store/{:032}-{name}.drv", name.len())
    }

    #[tokio::test]
    async fn run_supply_stage_prefetches_covered_paths_and_pauses_on_shortfall() {
        let (_dir, state) = state();
        let state = Arc::new(state);
        let fake = Arc::new(FakeSupplyTransport::default());
        // Ten prefetch-planned paths; the producing drvs of two of them are
        // scripted to fail, so 2/10 = 20% of the planned prefetch set is
        // missing at the end of the stage.
        let paths: Vec<String> = (0..10)
            .map(|index| format!("/nix/store/{:032}-prefetch{index}", index))
            .collect();
        {
            let mut scripted = fake.prefetch_results.lock().unwrap();
            for path in paths.iter().take(2) {
                scripted.insert(
                    producing_drv(path),
                    build_status_name(BuildStatus::PermanentFailure).to_string(),
                );
            }
        }
        let knobs = Knobs {
            prefetch_shortfall_pause_pct: 10.0,
            ..Knobs::default()
        };
        let mut inputs = prefetch_only_inputs(&paths);
        // One extra path already valid at the plan snapshot and one covered
        // path with no producing derivation: both are bookkeeping-only and
        // never join the planned prefetch set.
        let prior = "/nix/store/pppppppppppppppppppppppppppppppp-prior".to_string();
        let no_producer = "/nix/store/qqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqq-noproducer".to_string();
        inputs.prior_valid.insert(prior.clone());
        inputs.prefetch_paths.insert(prior.clone(), None);
        inputs.target_coverage.insert(prior.clone());
        inputs.prefetch_paths.insert(no_producer.clone(), None);
        inputs.target_coverage.insert(no_producer.clone());

        let report = run_supply_stage(
            state.clone(),
            fake.clone(),
            inputs,
            &knobs,
            Arc::new(AtomicU64::new(1)),
            // Tests never block on the operator: the PAUSE file is asserted
            // on directly instead of being waited for.
            false,
        )
        .await
        .unwrap();

        assert_eq!(report.planned_prefetch, 10);
        assert_eq!(report.prefetch_missing, 2);
        assert_eq!(report.shortfall_pct, Some(20.0));
        assert_eq!(report.delegated, 8);
        assert_eq!(report.already_present, 1);
        assert_eq!(report.unavailable, 1);
        assert!(
            state.path("PAUSE").exists(),
            "a 20% shortfall above the 10% threshold must create the PAUSE file"
        );
        let entries = entries(&state);
        assert_eq!(
            entry_for(&entries, &prior).outcome,
            SUPPLY_OUTCOME_ALREADY_PRESENT
        );
        assert_eq!(
            entry_for(&entries, &no_producer).outcome,
            SUPPLY_OUTCOME_UNAVAILABLE
        );

        // Shortfall zero (every prefetch succeeds): no PAUSE file.
        let (_dir2, state2) = state2();
        let state2 = Arc::new(state2);
        let healthy = Arc::new(FakeSupplyTransport::default());
        let report = run_supply_stage(
            state2.clone(),
            healthy,
            prefetch_only_inputs(&paths),
            &knobs,
            Arc::new(AtomicU64::new(1)),
            false,
        )
        .await
        .unwrap();
        assert_eq!(report.prefetch_missing, 0);
        assert_eq!(report.shortfall_pct, Some(0.0));
        assert!(
            !state2.path("PAUSE").exists(),
            "no shortfall, no PAUSE file"
        );
    }

    /// Second state-dir helper for tests that need two independent campaigns.
    fn state2() -> (tempfile::TempDir, StateDir) {
        state()
    }

    #[tokio::test]
    async fn run_supply_stage_uploads_drv_texts_and_records_policy_withheld() {
        // Self-hosted-shaped policy (`dependencies: none`) over the mini
        // archive: derivation texts are still uploaded, but dependency
        // outputs are withheld by policy and recorded as such from the
        // resolution itself (not as a generic planner skip).
        let archive_dir = tempfile::tempdir().unwrap();
        crate::run::archive_input::write_mini_archive(archive_dir.path());
        let archive =
            Arc::new(crate::archive::reader::ReplayArchive::open(archive_dir.path()).unwrap());
        let units = crate::run::archive_input::load_units(&archive).unwrap();
        let app_b = units
            .iter()
            .find(|unit| unit.job == "appB.x86_64-linux")
            .unwrap();

        let (_dir, state) = state();
        let state = Arc::new(state);
        let fake = Arc::new(FakeSupplyTransport::default());
        let inputs = SupplyInputs {
            workload_outputs: app_b.outputs.values().cloned().collect(),
            workload_drvs: BTreeSet::from([app_b.drv_path.clone()]),
            prefetch_paths: BTreeMap::new(),
            prior_valid: BTreeSet::new(),
            target_coverage: BTreeSet::new(),
            archive: Some(archive.clone()),
            target_substituters: Vec::new(),
            relay_substituters: Vec::new(),
            dependencies: SupplyDependencies::None,
            delivery: crate::run::spec::SupplyDelivery::Prewarm,
        };

        let report = run_supply_stage(
            state.clone(),
            fake.clone(),
            inputs,
            &Knobs::default(),
            Arc::new(AtomicU64::new(1)),
            false,
        )
        .await
        .unwrap();

        // The closure's three derivation texts (appB → libA → stdenv) were
        // uploaded; appB's own outputs are never supplied and never recorded.
        assert_eq!(report.delivered, 3);
        let entries = entries(&state);
        assert_eq!(
            entry_for(&entries, &app_b.drv_path).outcome,
            SUPPLY_OUTCOME_DELIVERED
        );
        for output in app_b.outputs.values() {
            assert!(
                entries.iter().all(|entry| &entry.path != output),
                "workload output {output} must not get a supply entry"
            );
        }
        // The two dependency outputs (libA's and stdenv's) are withheld by
        // the `none` dependency policy, recorded unavailable with a detail
        // naming the policy, and counted in the report.
        let withheld: Vec<&SupplyEntry> = entries
            .iter()
            .filter(|entry| entry.outcome == SUPPLY_OUTCOME_UNAVAILABLE)
            .collect();
        assert_eq!(withheld.len(), 2, "{withheld:?}");
        assert_eq!(report.unavailable, 2);
        for entry in withheld {
            assert_eq!(entry.source, crate::run::model::SUPPLY_SOURCE_NONE);
            assert_eq!(entry.mechanism, crate::run::model::SUPPLY_MECHANISM_NONE);
            assert!(
                entry
                    .detail
                    .as_deref()
                    .unwrap_or_default()
                    .contains("policy"),
                "{entry:?}"
            );
        }
        // No prefetch was planned, so the shortfall gate does not apply.
        assert_eq!(report.planned_prefetch, 0);
        assert_eq!(report.shortfall_pct, None);
        assert!(!state.path("PAUSE").exists());
    }
}
