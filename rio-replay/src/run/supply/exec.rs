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
//!   what the given roots' closure still misses. [`LadderTopup`] packages it
//!   with the stage's ladder context as the [`PreSubmitSupply`] hook the
//!   execute stage calls before each submission — per request in the timed
//!   dispatcher, per batch in the timeless submit loop.
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
    PathOutcome, SUPPLY_DETAIL_DEFERRED_INLINE, SUPPLY_MECHANISM_DELEGATE, SUPPLY_MECHANISM_NONE,
    SUPPLY_MECHANISM_UPLOAD_BATCH, SUPPLY_MECHANISM_UPLOAD_STREAM, SUPPLY_OUTCOME_ALREADY_PRESENT,
    SUPPLY_OUTCOME_DELEGATED, SUPPLY_OUTCOME_DELIVERED, SUPPLY_OUTCOME_FAILED,
    SUPPLY_OUTCOME_REFUSED, SUPPLY_OUTCOME_SKIPPED, SUPPLY_OUTCOME_UNAVAILABLE,
    SUPPLY_SOURCE_EMBEDDED, SUPPLY_SOURCE_NONE, SUPPLY_SOURCE_RELAY,
    SUPPLY_SOURCE_TARGET_SUBSTITUTER, SupplyEntry, build_status_from_name, now_rfc3339,
    path_outcomes_from_keyed, supply_outcome_is_settlement,
};
use crate::run::spec::{Knobs, SupplyDelivery, SupplyDependencies};
use crate::run::state::{StateDir, StateFile};
use crate::run::transport::{DaemonChannel, GatewayPool, TransportError};
use crate::substituter::Substituter;

use super::{
    ClaimOutcome, PathSource, UploadClaims, UploadItem, UploadPayload, UploadPlan, plan_uploads,
    resolve_source, split_batches, topo_levels, walk_closure,
};

/// Base deadline for individually streamed (large) uploads. Probes and
/// batch uploads use the spec's `op_timeout_secs` as their base; a multi-GB
/// NAR legitimately needs longer even before payload headroom. The wire ops
/// add payload-proportional headroom on top of whichever base they are
/// given (`DaemonChannel::{add_multiple_to_store,add_to_store_nar}` derive
/// the effective deadline from the entries' NAR bytes), so neither arm's
/// base needs to anticipate payload size.
const LARGE_UPLOAD_TIMEOUT: Duration = Duration::from_secs(600);

/// Skip detail recorded for uploads this invocation did not attempt because
/// the gateway circuit breaker was open. Recorded under
/// [`SUPPLY_OUTCOME_SKIPPED`], never as a failure: no transport call was
/// made, so nothing about the path's delivery settled here.
const GATEWAY_UNREACHABLE: &str = "gateway unreachable; not attempted";

/// Skip detail recorded when another request still holds a path's upload
/// claim (after the bounded `claim_wait_mins` wait and the single re-claim,
/// per the cross-request claims contract) and this invocation proceeds
/// without the path. The claim HOLDER settles the path — its eventual
/// `delivered`/`refused`/`failed` row is the authoritative outcome — so
/// this row is per-request evidence of the gap, not a settlement.
const CLAIM_STILL_HELD: &str = "upload claim still held by another request; proceeded without it";

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
/// the inline-delivery path). The production implementation is
/// [`LadderTopup`], which wraps [`topup_for_roots`] over the supply stage's
/// ladder context.
#[async_trait]
pub trait PreSubmitSupply: Send + Sync {
    /// Top up the target store for the given root derivations.
    async fn topup(&self, roots: &[String]) -> anyhow::Result<()>;
}

/// Production [`PreSubmitSupply`]: the supply stage's ladder context and
/// transport packaged for the execute stage (the timed dispatcher's
/// per-request call, the timeless submit loop's per-batch call), so every
/// submission gets a [`topup_for_roots`] pass over its root derivations
/// immediately beforehand — the delivery mechanism itself under inline
/// delivery, and under prewarm the top-up that backstops a prewarm miss (a
/// path the prewarm pass refused, failed, or never planned).
///
/// Cheap when there is nothing to do: the held [`SupplyContext`] is the one
/// the supply stage probed and uploaded with, so paths already delivered (by
/// the prewarm pass or an earlier top-up) or already probed valid are never
/// re-sent — a fully supplied request costs one validity probe of its
/// closure remainder and uploads nothing. Concurrent requests needing the
/// same path coordinate through the held [`UploadClaims`]. Reusing the
/// stage's context also means the substituter guard is never re-run here:
/// only URLs the stage already admitted can be fetched from.
pub struct LadderTopup {
    /// The supply stage's transport (build-tenant probes and uploads).
    transport: Arc<dyn SupplyTransport>,
    /// The open replay archive (payload source for embedded paths).
    archive: Arc<ReplayArchive>,
    /// The supply stage's ladder context: admitted relay substituters,
    /// probed coverage, and the evolving target-validity set.
    ctx: SupplyContext,
    /// Campaign knobs (batch caps, timeouts, claim wait).
    knobs: Knobs,
    /// Campaign state dir for supply.jsonl appends.
    state: Arc<StateDir>,
    /// Cross-request upload claims shared by every top-up call.
    claims: UploadClaims,
}

impl LadderTopup {
    /// Package the supply stage's transport and ladder context for
    /// per-request top-up calls.
    pub fn new(
        transport: Arc<dyn SupplyTransport>,
        archive: Arc<ReplayArchive>,
        ctx: SupplyContext,
        knobs: Knobs,
        state: Arc<StateDir>,
    ) -> Self {
        Self {
            transport,
            archive,
            ctx,
            knobs,
            state,
            claims: UploadClaims::new(),
        }
    }
}

#[async_trait]
impl PreSubmitSupply for LadderTopup {
    async fn topup(&self, roots: &[String]) -> anyhow::Result<()> {
        topup_for_roots(
            self.transport.as_ref(),
            &self.archive,
            &self.ctx,
            roots,
            &self.knobs,
            &self.state,
            &self.claims,
        )
        .await
    }
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
    /// Prefetch-wanted paths that could not be planned for prefetch at all
    /// AND that no ladder source could provide either (no upstream
    /// coverage / no producing derivation / nothing on the ladder): part
    /// of the shortfall denominators alongside the planned set, so a
    /// largely-undeliverable prefetch set (an aged-out upstream) reads as
    /// a shortfall instead of silently shrinking what the gate measures.
    pub prefetch_unavailable: usize,
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
    /// Planned uploads skipped without an attempt (circuit breaker open, or
    /// the path's upload claim still held by another request). Nothing
    /// settled for these paths in the recording invocation: they remain
    /// deliverable by a later top-up or run, and they retire nobody.
    pub skipped: usize,
    /// Total uncompressed NAR bytes uploaded by the engine.
    pub uploaded_bytes: u64,
    /// Wall-clock seconds spent in the upload arms.
    pub upload_secs: f64,
    /// Sustained engine upload throughput in MiB/s; `None` when nothing was
    /// uploaded.
    pub upload_mib_per_s: Option<f64>,
    /// Substituter narinfo-probe failure counts by cache URL.
    pub probe_errors: BTreeMap<String, u64>,
    /// Prefetch shortfall percentage:
    /// (missing + unavailable) / (planned + unavailable) × 100. `None`
    /// when the prefetch policy wanted nothing at all; an all-unavailable
    /// wanted set yields 100, never a skipped gate.
    pub shortfall_pct: Option<f64>,
    /// The prewarm upload pass ended with its gateway circuit breaker
    /// tripped: the remaining planned uploads were skipped without an
    /// attempt (recorded `skipped`, retiring nobody), so the campaign
    /// would otherwise start execution with its planned supply largely
    /// undelivered. Gated behind the campaign PAUSE file by
    /// [`run_supply_stage`], exactly like the prefetch shortfall — whether
    /// to accept the gap or abort is an operator decision, never an
    /// engine heuristic. Defaults false on reports written before the
    /// flag existed.
    pub upload_collapsed: bool,
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
    /// Upstream coverage already known per path (the warm-set
    /// upstream-coverage probe's found paths); the supply stage's own
    /// narinfo probes extend it for the upload ladder.
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
    /// Base deadline for probes and batch uploads (streamed uploads use
    /// [`LARGE_UPLOAD_TIMEOUT`] as their base); the upload wire ops add
    /// payload-proportional headroom themselves.
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
            // The mapping checks the daemon's result count against the
            // submitted roots and warns on a mismatch; uncovered roots are
            // handled by the prefetch arm's missing-result rule.
            Ok(results) => Ok(path_outcomes_from_keyed(roots, &results)),
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
/// the invocation and remaining work is recorded `skipped` without further
/// transport calls — bookkeeping, never settled `failed` rows: a breaker
/// skip is not a delivery attempt, so it must neither retire dependents nor
/// contradict a delivery another request already made. The per-submission
/// top-up gets a fresh breaker per invocation, so skipped paths stay
/// re-attemptable once the gateway returns.
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
    /// Paths whose delivery attempt failed (transport, materialization,
    /// failed reference).
    failed: usize,
    /// Paths skipped without an attempt (breaker open, claim held
    /// elsewhere) — nothing settled for them in this invocation.
    skipped: usize,
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
///
/// `skipped` rows are bookkeeping, not settlements: they count under their
/// own total and never poison dependents — the path's delivery was not
/// resolved here (the claim holder, a later top-up, or a re-run settles
/// it), so pre-failing its referrers would mint settled `failed` rows for
/// paths nothing actually attempted.
///
/// Resolution/append ordering: settled rows are minted only by the path's
/// claim winner, and the append must happen while the claim resolution
/// that authorizes the row is still in force — asserted here, at the one
/// chokepoint every upload-arm row passes through, so the ordering is a
/// checked invariant instead of a per-call-site convention:
///
/// - refused/failed: the claim must still be PENDING. Releasing first
///   would wake a parked sibling that can re-claim, upload, and append
///   its `delivered` row before this failure lands — the journal would
///   read delivered-then-failed and the last-settlement-wins rollup would
///   falsely retire a delivered path's dependents (the exact inversion
///   the claims-first contract above [`upload_stream_one`] forbids).
/// - delivered: the claim must already be DONE ([`UploadClaims::complete`]
///   precedes the append), so a sibling waking on the completed claim can
///   never observe a landed path with no delivered row.
/// - skipped rows carry no claim authority either way: a held-claim skip
///   is recorded while a SIBLING holds the claim, a breaker skip after
///   this invocation released its own, and bookkeeping rows can neither
///   retire dependents nor displace a settlement under the journal's
///   folds.
fn record_settlement(
    env: &UploadEnv<'_>,
    item: &UploadItem,
    mechanism: &'static str,
    outcome: &'static str,
    detail: Option<String>,
    batch_id: u64,
    bytes: Option<u64>,
) -> Result<()> {
    match outcome {
        SUPPLY_OUTCOME_REFUSED | SUPPLY_OUTCOME_FAILED => debug_assert!(
            env.claims.is_pending(&item.store_path),
            "settled {outcome} row for {} appended without its upload claim held: \
             release the claim only AFTER the row is recorded",
            item.store_path
        ),
        SUPPLY_OUTCOME_DELIVERED => debug_assert!(
            env.claims.is_done(&item.store_path),
            "settled delivered row for {} appended before its claim completed",
            item.store_path
        ),
        _ => {}
    }
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
    } else if outcome == SUPPLY_OUTCOME_SKIPPED {
        totals.skipped += 1;
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
    /// Another request still holds the claim; proceed without the path,
    /// recording a `skipped` bookkeeping row — the holder's own settlement
    /// is the path's authoritative outcome.
    SkipHeld,
}

/// Claim `path` for upload, waiting (bounded) for another holder's claim and
/// re-claiming exactly once if that claim is released without landing.
///
/// This is the cross-request claims contract: claim, wait up to the
/// configured `claim_wait_mins`, re-claim exactly once, then proceed
/// without the path. The wait is the ONLY blocking step and it is
/// deadline-bounded — settlement of a held path is always the holder's job,
/// never an open-ended wait here.
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

/// Non-blocking [`ClaimDecision`] for arms that must not wait (the breaker
/// is open and the invocation is wrapping up): the claims table is still
/// consulted — it is the one place that knows whether another request
/// already delivered or is still uploading the path — but a held claim is
/// skipped immediately instead of waited on. A `Upload` decision here means
/// this invocation won the claim; the caller releases it after recording
/// its skip so a later top-up can re-claim the path.
fn claim_decision_nowait(env: &UploadEnv<'_>, path: &str) -> ClaimDecision {
    match env.claims.claim(path) {
        ClaimOutcome::Won => ClaimDecision::Upload,
        ClaimOutcome::AlreadyDone => ClaimDecision::SkipLanded,
        ClaimOutcome::MustWait => ClaimDecision::SkipHeld,
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
///
/// Deadline scoping is per arm, matching what each arm actually awaits on.
/// The relay arm's `fetch_nar_streaming` returns at response headers (both
/// substituter arms hand back an unconsumed body reader; the body is drained
/// later, inside the wire op's payload-scaled deadline), so `env.op_timeout`
/// bounds only a metadata-scale header wait — without it, a cache that
/// completes TLS but never sends response headers pends this await forever,
/// wedging prewarm workers and both pre-submit top-up paths with no breaker,
/// watchdog, or settlement signal. The local arm (embedded/drv-text) instead
/// materializes the FULL NAR in memory before returning; for streamed items
/// that is payload-scale local work, so a flat metadata deadline around the
/// whole call would mass-fail healthy multi-GB embedded paths — which is why
/// the timeout wraps only the relay fetch, not the function.
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
            match tokio::time::timeout(env.op_timeout, substituter.fetch_nar_streaming(narinfo))
                .await
            {
                Ok(Ok((len, reader))) => Ok(NarPayload::Reader { len, reader }),
                Ok(Err(err)) => Err(format!("relay fetch failed: {err:#}")),
                Err(_elapsed) => Err(format!(
                    "relay fetch did not return response headers within {}s",
                    env.op_timeout.as_secs()
                )),
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
///
/// The cross-request claims table is consulted FIRST, before any
/// breaker-based skip: the table (and the validity set it feeds) is the one
/// place that knows whether another request already delivered the path, so
/// any row recorded before consulting it could contradict the journal's
/// settled truth — a `failed` row appended after a sibling request's
/// `delivered` row would become the path's last settlement and falsely
/// retire every dependent on the next rollup. Settled rows are minted only
/// through claim resolution; skips (breaker open, claim held) record
/// `skipped` bookkeeping rows that settle nothing.
///
/// The contract has an ordering half on the failure arms: the
/// refused/failed row is appended BEFORE the claim is released. Release
/// wakes any sibling parked on the claim; if it preceded the append, that
/// sibling could re-claim, upload, and append its `delivered` row first —
/// writing the same forbidden delivered-then-failed sequence through the
/// timing side door. [`record_settlement`] asserts the ordering at the
/// append chokepoint.
async fn upload_stream_one(env: &UploadEnv<'_>, item: &UploadItem) -> Result<()> {
    let batch_id = env.sub_batch_ids.fetch_add(1, Ordering::SeqCst);
    let mechanism = SUPPLY_MECHANISM_UPLOAD_STREAM;
    let decision = if env.breaker.is_tripped() {
        // No waiting on held claims while the gateway is gone — but landed
        // paths must still be recognized, never re-recorded as anything.
        claim_decision_nowait(env, &item.store_path)
    } else {
        claim_for_upload(env, &item.store_path).await
    };
    match decision {
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
            // The bounded wait (or the breaker fast path) expired with the
            // claim still held: proceed without the path, leaving a
            // bookkeeping row so the gap is attributable per request. The
            // holder settles the path.
            return record_settlement(
                env,
                item,
                mechanism,
                SUPPLY_OUTCOME_SKIPPED,
                Some(CLAIM_STILL_HELD.to_string()),
                batch_id,
                None,
            );
        }
    }
    // Claim won. The breaker may be open (it was open before the
    // non-blocking claim, or tripped while a bounded claim wait ran): skip
    // without a transport attempt and release the claim so a later top-up
    // can re-claim. Skipped, not failed — no attempt was made, so nothing
    // about the path's delivery settled.
    if env.breaker.is_tripped() {
        env.claims.release(&item.store_path);
        return record_settlement(
            env,
            item,
            mechanism,
            SUPPLY_OUTCOME_SKIPPED,
            Some(GATEWAY_UNREACHABLE.to_string()),
            batch_id,
            None,
        );
    }
    if let Some(reference) = failed_reference_of(env, item) {
        // Row first, then release (on append success and failure alike,
        // so an append error never leaks the claim) — see
        // [`record_settlement`]'s ordering contract.
        let recorded = record_settlement(
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
        env.claims.release(&item.store_path);
        return recorded;
    }

    // First attempt, then exactly one retry when the daemon refused (the
    // refusal may be genuine or a transport error racing a refusal; one
    // retry on a fresh channel distinguishes a flake from a real rejection).
    let mut refused: Option<String> = None;
    for attempt in 0..2 {
        let payload = match materialize_stream_payload(env, item).await {
            Ok(payload) => payload,
            Err(detail) => {
                // Row first, then release — record_settlement's contract.
                let recorded = record_settlement(
                    env,
                    item,
                    mechanism,
                    SUPPLY_OUTCOME_FAILED,
                    Some(detail),
                    batch_id,
                    None,
                );
                env.claims.release(&item.store_path);
                return recorded;
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
                // Row first, then release — record_settlement's contract.
                let recorded = record_settlement(
                    env,
                    item,
                    mechanism,
                    SUPPLY_OUTCOME_FAILED,
                    Some(format!("streamed upload failed: {err:#}")),
                    batch_id,
                    None,
                );
                env.claims.release(&item.store_path);
                return recorded;
            }
        }
    }
    // Refused after the retry: row first, then release.
    let recorded = record_settlement(
        env,
        item,
        mechanism,
        SUPPLY_OUTCOME_REFUSED,
        refused,
        batch_id,
        None,
    );
    env.claims.release(&item.store_path);
    recorded
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
                // Row first, then release — record_settlement's contract.
                let recorded = record_settlement(
                    env,
                    item,
                    SUPPLY_MECHANISM_UPLOAD_BATCH,
                    SUPPLY_OUTCOME_FAILED,
                    Some(detail),
                    batch_id,
                    None,
                );
                env.claims.release(&item.store_path);
                recorded?;
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
/// batch attempt, then release their claims — per-item row first, release
/// second ([`record_settlement`]'s ordering contract): while the claim is
/// pending no sibling can upload the path, so its `delivered` row can
/// never precede this settled failure in the journal.
fn settle_batch_undelivered(
    env: &UploadEnv<'_>,
    items: &[&UploadItem],
    outcome: &'static str,
    detail: &str,
    batch_id: u64,
) -> Result<()> {
    for item in items {
        let recorded = record_settlement(
            env,
            item,
            SUPPLY_MECHANISM_UPLOAD_BATCH,
            outcome,
            Some(detail.to_string()),
            batch_id,
            None,
        );
        // Released on append success and failure alike: an append error
        // aborts the invocation but must not leak this item's claim.
        env.claims.release(&item.store_path);
        recorded?;
    }
    Ok(())
}

/// Upload one sub-batch: claims, failed-reference pre-skip, one wire
/// attempt, and exactly one retry on a fresh channel when the daemon
/// refused.
///
/// As in [`upload_stream_one`], the cross-request claims table is consulted
/// FIRST, before any breaker-based skip: settled rows are minted only
/// through claim resolution, so a path another request already delivered
/// can never receive a contradicting row from this arm, and skips record
/// `skipped` bookkeeping rows that settle nothing.
async fn upload_sub_batch(env: &UploadEnv<'_>, items: Vec<&UploadItem>) -> Result<()> {
    let batch_id = env.sub_batch_ids.fetch_add(1, Ordering::SeqCst);
    // Cross-request claims: only paths this invocation wins are sent here.
    // Under an open breaker the table is read without waiting on held
    // claims — the invocation is wrapping up, not coordinating.
    let tripped_at_claims = env.breaker.is_tripped();
    let mut kept: Vec<&UploadItem> = Vec::with_capacity(items.len());
    for item in items {
        let decision = if tripped_at_claims {
            claim_decision_nowait(env, &item.store_path)
        } else {
            claim_for_upload(env, &item.store_path).await
        };
        match decision {
            ClaimDecision::Upload => kept.push(item),
            ClaimDecision::SkipLanded => {
                env.ctx
                    .target_valid
                    .write()
                    .await
                    .insert(item.store_path.clone());
            }
            ClaimDecision::SkipHeld => {
                record_settlement(
                    env,
                    item,
                    SUPPLY_MECHANISM_UPLOAD_BATCH,
                    SUPPLY_OUTCOME_SKIPPED,
                    Some(CLAIM_STILL_HELD.to_string()),
                    batch_id,
                    None,
                )?;
            }
        }
    }
    // Breaker check for the claim winners — including a re-check after the
    // bounded claim waits above, which can outlast a gateway collapse:
    // release each claim and record a `skipped` bookkeeping row. No
    // transport attempt was made for these paths, so nothing settled.
    if env.breaker.is_tripped() {
        for item in &kept {
            env.claims.release(&item.store_path);
            record_settlement(
                env,
                item,
                SUPPLY_MECHANISM_UPLOAD_BATCH,
                SUPPLY_OUTCOME_SKIPPED,
                Some(GATEWAY_UNREACHABLE.to_string()),
                batch_id,
                None,
            )?;
        }
        return Ok(());
    }
    // An item whose reference already failed to upload would only be refused
    // by the daemon; skip it up front naming the real culprit. Levels run in
    // order, so this also covers transitive dependents in later levels.
    let mut to_send: Vec<&UploadItem> = Vec::with_capacity(kept.len());
    for item in kept {
        match failed_reference_of(env, item) {
            Some(reference) => {
                // Row first, then release — record_settlement's contract.
                let recorded = record_settlement(
                    env,
                    item,
                    SUPPLY_MECHANISM_UPLOAD_BATCH,
                    SUPPLY_OUTCOME_FAILED,
                    Some(format!(
                        "reference {reference} failed its earlier upload — skipped"
                    )),
                    batch_id,
                    None,
                );
                env.claims.release(&item.store_path);
                recorded?;
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
        skipped = totals.skipped,
        uploaded_mib = totals.uploaded_bytes / (1024 * 1024),
        elapsed_s = upload_secs,
        "prewarm uploads finished"
    );
    Ok(SupplyStageReport {
        delivered: totals.delivered,
        refused: totals.refused,
        failed: totals.failed,
        skipped: totals.skipped,
        uploaded_bytes: totals.uploaded_bytes,
        upload_secs,
        upload_mib_per_s,
        upload_collapsed: breaker.is_tripped(),
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
        skipped = totals.skipped,
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
/// inline delivery). Folds its outcomes into `report` and returns the ladder
/// context it built (admitted relay substituters, probed coverage, the
/// validity set as of the last delivery) so the caller can keep it for
/// per-request top-ups; `None` when the workload has no drvs to plan over.
async fn run_upload_ladder(
    state: &StateDir,
    transport: &dyn SupplyTransport,
    archive: &Arc<ReplayArchive>,
    inputs: &SupplyInputs,
    knobs: &Knobs,
    report: &mut SupplyStageReport,
    prefetch_unavailable: &mut BTreeSet<String>,
) -> Result<Option<SupplyContext>> {
    let roots: Vec<String> = inputs.workload_drvs.iter().cloned().collect();
    if roots.is_empty() {
        return Ok(None);
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
        if inputs.prefetch_paths.contains_key(path) {
            // Prefetch-wanted paths carry their own journal bookkeeping
            // (the upstream-coverage probe and the prefetch
            // classification); here the ladder only settles their
            // shortfall membership: a path the ladder CAN source
            // (embedded / relay / target substituter) is deliverable
            // after all and leaves the unavailable side of the
            // denominators, while a NotSupplied one stays — the
            // mostly-uncoverable warm set is exactly the case the
            // pre-execution gate exists for.
            if source != (PathSource::NotSupplied { workload: false }) {
                prefetch_unavailable.remove(path);
            }
        } else if source == (PathSource::NotSupplied { workload: false }) {
            let detail =
                if inputs.dependencies == SupplyDependencies::None && !input_srcs.contains(path) {
                    "dependency output withheld by the supply policy (dependencies = \"none\")"
                } else {
                    "no target substituter, archive member, or relay substituter can provide \
                     this path"
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
            report.skipped = upload.skipped;
            report.uploaded_bytes = upload.uploaded_bytes;
            report.upload_secs = upload.upload_secs;
            report.upload_mib_per_s = upload.upload_mib_per_s;
            report.upload_collapsed = upload.upload_collapsed;
        }
        SupplyDelivery::Inline => {
            // Inline delivery defers planned uploads to the per-submission
            // top-up; the deferral is recorded so the report shows what was
            // deliberately not delivered before execution, and so the
            // inline-resume gate can tell a stage that delivered (prewarm)
            // from one that only promised (this arm) after the process —
            // and with it the top-up context — is gone. A later top-up that
            // delivers/refuses/fails the path supersedes the deferral under
            // the journal's latest-row-per-path reading.
            for item in plan.large.iter().chain(plan.batch.iter()) {
                append_supply_entry(
                    state,
                    &item.store_path,
                    entry_source(item),
                    SUPPLY_MECHANISM_NONE,
                    SUPPLY_OUTCOME_UNAVAILABLE,
                    Some(SUPPLY_DETAIL_DEFERRED_INLINE.to_string()),
                )?;
                report.unavailable += 1;
            }
        }
    }
    Ok(Some(ctx))
}

/// What [`run_supply_stage`] hands back to the campaign orchestrator: the
/// stage report plus the ladder context the upload ladder built, kept alive
/// so the execute stage's pre-submission top-up ([`LadderTopup`]) can reuse
/// the already-admitted substituters and probe results instead of
/// re-admitting or re-probing them.
pub struct SupplyStageOutput {
    /// Aggregated stage accounting (persisted as `supply-report.json`).
    pub report: SupplyStageReport,
    /// The upload ladder's context; `None` when the stage ran without an
    /// archive or the workload had no drvs to plan over.
    pub ladder: Option<SupplyContext>,
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
) -> Result<SupplyStageOutput> {
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
    // Prefetch-wanted paths that cannot be prefetched OR ladder-sourced —
    // the shortfall denominators' unavailable side. A set (not a counter)
    // because the classification arms here and the ladder's resolve loop
    // can both see the same path; it must count once.
    let mut prefetch_unavailable: BTreeSet<String> = BTreeSet::new();
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
            // still deliver it — the path enters the unavailable side of
            // the shortfall denominators provisionally and leaves it again
            // if the ladder finds a source; without a ladder pass nothing
            // can deliver it and it stays.
            (false, Some(_)) => {
                prefetch_unavailable.insert(path.clone());
            }
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
                prefetch_unavailable.insert(path.clone());
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
                prefetch_unavailable.insert(path.clone());
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
    let mut ladder = None;
    if let Some(archive) = inputs.archive.clone() {
        ladder = run_upload_ladder(
            &state,
            transport.as_ref(),
            &archive,
            &inputs,
            knobs,
            &mut report,
            &mut prefetch_unavailable,
        )
        .await?;
    }

    // ── Upload-collapse gate ────────────────────────────────────────────
    // The prewarm pass ended with its circuit breaker tripped: everything
    // it had not yet sent was skipped without an attempt (bookkeeping
    // rows — nothing settled, nothing retires), so the campaign would
    // start execution with its planned supply largely undelivered and
    // only the per-submission top-up between every affected unit and a
    // missing-input failure. Like the prefetch shortfall below, that is
    // an operator decision: pause before the execution clock starts —
    // removing the PAUSE file accepts the gap (the top-up re-attempts
    // skipped paths against a fresh breaker), re-running the stage after
    // deleting the supply marker re-prewarms, aborting costs nothing.
    if report.upload_collapsed {
        tracing::error!(
            delivered = report.delivered,
            failed = report.failed,
            skipped = report.skipped,
            "the prewarm upload circuit breaker tripped (gateway unreachable); the campaign is \
             paused before execution — remove the PAUSE file to proceed on the per-submission \
             top-up alone, or abort"
        );
        pause_campaign(&state, "supply upload collapse", knobs, wait_for_resume).await?;
    }

    // ── Prefetch shortfall gate ─────────────────────────────────────────
    // The denominator is the whole wanted-but-not-yet-present set: planned
    // prefetches plus the paths that could not be planned (or ladder-
    // sourced) at all. Without the unavailable side, a wanted set that is
    // MOSTLY undeliverable (aged-out upstream, floating CA outputs — the
    // durability case this gate exists for) would shrink the denominator
    // toward zero and the campaign would start execution silently
    // under-supplied, with neither the pause nor the low-confidence flag
    // able to fire.
    report.prefetch_unavailable = prefetch_unavailable.len();
    let prefetch_wanted = report.planned_prefetch + report.prefetch_unavailable;
    if prefetch_wanted > 0 {
        let shortfall_pct = (report.prefetch_missing + report.prefetch_unavailable) as f64
            / prefetch_wanted as f64
            * 100.0;
        report.shortfall_pct = Some(shortfall_pct);
        if shortfall_pct > knobs.prefetch_shortfall_pause_pct {
            tracing::error!(
                prefetch_missing = report.prefetch_missing,
                prefetch_unavailable = report.prefetch_unavailable,
                planned_prefetch = report.planned_prefetch,
                shortfall_pct,
                threshold_pct = knobs.prefetch_shortfall_pause_pct,
                "prefetch shortfall above the pause threshold; the campaign is paused before \
                 execution — remove the PAUSE file to accept the shortfall and resume, or abort"
            );
            pause_campaign(&state, "prefetch shortfall", knobs, wait_for_resume).await?;
        }
    }

    Ok(SupplyStageOutput { report, ladder })
}

/// Write the campaign PAUSE file naming `reason` and (when
/// `wait_for_resume`) block until an operator removes it — the shared
/// chokepoint of the pre-execution supply gates (prefetch shortfall,
/// upload collapse), so every gate pauses the same way and the execution
/// clock never starts on a silently under-supplied campaign.
async fn pause_campaign(
    state: &StateDir,
    reason: &str,
    knobs: &Knobs,
    wait_for_resume: bool,
) -> Result<()> {
    let pause = state.path("PAUSE");
    std::fs::write(&pause, format!("{reason}\n"))
        .with_context(|| format!("write {}", pause.display()))?;
    if wait_for_resume {
        let poll = Duration::from_secs(knobs.cluster_status_poll_secs.max(1));
        while state.path("PAUSE").exists() {
            tokio::time::sleep(poll).await;
        }
        tracing::info!(reason, "PAUSE file removed; resuming");
    }
    Ok(())
}

/// Re-derive the per-path outcome counts of a [`SupplyStageReport`] from the
/// supply journal, keeping only the latest SETTLEMENT record per path.
///
/// The journal legitimately carries more than one row for a path — the
/// upstream-coverage probe records `unavailable` before the supply stage
/// runs, and a path one arm could not provide can be delivered by a later
/// arm or a top-up — so counting raw rows would double-count. The last
/// settlement row per path is its settled disposition; bookkeeping rows
/// (`unavailable`, `skipped` — see [`supply_outcome_is_settlement`])
/// count only for paths that never settled, because they assert nothing
/// about delivery: a skip-held row appended after the claim holder's
/// `delivered` row must leave the path counted delivered, and a breaker
/// skip after a real failure must leave it counted failed. Throughput,
/// prefetch-shortfall (including the prefetch-scoped unavailable tally —
/// the journal cannot distinguish prefetch-wanted unavailability from
/// ordinary ladder unavailability), and probe-error figures are not
/// per-path counts and stay as the stage reported them. An empty journal
/// leaves the report untouched.
pub fn refresh_outcome_counts(report: &mut SupplyStageReport, entries: &[SupplyEntry]) {
    if entries.is_empty() {
        return;
    }
    let mut latest: BTreeMap<&str, &str> = BTreeMap::new();
    for entry in entries {
        let outcome = entry.outcome.as_str();
        latest
            .entry(entry.path.as_str())
            .and_modify(|current| {
                // A settlement always supersedes; bookkeeping supersedes
                // only bookkeeping.
                if supply_outcome_is_settlement(outcome) || !supply_outcome_is_settlement(current) {
                    *current = outcome;
                }
            })
            .or_insert(outcome);
    }
    let count = |outcome: &str| latest.values().filter(|got| **got == outcome).count();
    report.delivered = count(SUPPLY_OUTCOME_DELIVERED);
    report.delegated = count(SUPPLY_OUTCOME_DELEGATED);
    report.already_present = count(SUPPLY_OUTCOME_ALREADY_PRESENT);
    report.refused = count(SUPPLY_OUTCOME_REFUSED);
    report.unavailable = count(SUPPLY_OUTCOME_UNAVAILABLE);
    report.failed = count(SUPPLY_OUTCOME_FAILED);
    report.skipped = count(SUPPLY_OUTCOME_SKIPPED);
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
    use crate::run::model::build_status_name;

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
        /// Roots whose entry is omitted from `prefetch_build`'s answer — the
        /// shape a daemon answering fewer results than submitted roots
        /// leaves behind after the positional mapping.
        pub prefetch_omitted: Mutex<BTreeSet<String>>,
        /// When set, every upload fails with [`SupplyTransportError::Other`]
        /// (the shape of a channel-open failure).
        pub fail_uploads: AtomicBool,
        /// Paths whose every upload fails with
        /// [`SupplyTransportError::Other`] (a per-path hard transport
        /// failure, without the run-wide collapse `fail_uploads` scripts —
        /// lets a test settle ONE path `failed` while its siblings deliver
        /// and the breaker stays closed).
        pub fail_paths: Mutex<BTreeSet<String>>,
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
            {
                let fail_paths = self.fail_paths.lock().unwrap();
                if let Some(path) = paths.iter().find(|path| fail_paths.contains(*path)) {
                    return Err(SupplyTransportError::Other(anyhow!(
                        "scripted per-path transport failure for {path}"
                    )));
                }
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
            let omitted = self.prefetch_omitted.lock().unwrap();
            Ok(roots
                .iter()
                .filter(|drv| !omitted.contains(*drv))
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
    use crate::run::model::build_status_name;
    use rio_nix::protocol::build::BuildResult;
    use rio_nix::protocol::client::KeyedBuildResult;

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

    /// A relay cache that completes the TCP handshake but never sends
    /// response headers must not wedge the streamed arm: the relay
    /// materialization's header-phase wait is bounded by `op_timeout`
    /// (scoped to headers only — the body is consumed later, inside the
    /// wire op's payload-scaled deadline; the local materialization arm
    /// is deliberately outside this bound, see
    /// `materialize_stream_payload`). The item settles FAILED with a
    /// detail naming the header wait, instead of pending forever with
    /// no breaker, watchdog, or settlement signal. The outer 30s guard
    /// is the red-state detector: before the header-phase deadline
    /// existed, this await never returned.
    #[tokio::test]
    async fn streamed_relay_header_stall_settles_failed_within_op_timeout() {
        // A "cache" that accepts connections and then goes silent, holding
        // the socket open without ever writing a response.
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let mut held = Vec::new();
            loop {
                let Ok((socket, _)) = listener.accept().await else {
                    break;
                };
                held.push(socket);
            }
        });

        let (_dir, state) = state();
        let fake = FakeSupplyTransport::default();
        let mut ctx = SupplyContext::new(SupplyDependencies::Substituters);
        let substituter = Substituter::parse(&format!("http://{addr}")).await.unwrap();
        let canonical = substituter.url();
        // Insert directly: the public-cache admission guard (which would
        // refuse a loopback URL) is not under test here.
        ctx.relay.insert(canonical.clone(), substituter);

        let narinfo = NarInfo {
            store_path: PATH_A.to_string(),
            url: "nar/stalled.nar".into(),
            compression: "none".into(),
            nar_hash: "sha256:0000000000000000000000000000000000000000000000000000".into(),
            nar_size: 1024,
            references: Vec::new(),
            deriver: None,
            sigs: Vec::new(),
            ca: None,
            file_hash: None,
            file_size: None,
        };
        let relay_item = UploadItem {
            store_path: PATH_A.to_string(),
            info: item(PATH_A, vec![0u8; 1024], &[]).info,
            payload: UploadPayload::Relay {
                substituter_url: canonical,
                narinfo,
            },
        };
        let plan = UploadPlan {
            large: vec![relay_item],
            batch: Vec::new(),
            skipped: Vec::new(),
        };
        let knobs = Knobs {
            op_timeout_secs: 1,
            ..Knobs::default()
        };
        let claims = UploadClaims::new();

        let report = tokio::time::timeout(
            Duration::from_secs(30),
            prewarm_uploads(&fake, None, &ctx, &plan, &knobs, &state, &claims),
        )
        .await
        .expect(
            "a headers-withholding relay cache must be bounded by the header-phase deadline, \
             not wedge prewarm forever",
        )
        .unwrap();

        assert_eq!(report.failed, 1, "the stalled relay item settles failed");
        assert!(
            fake.uploaded_streamed.lock().unwrap().is_empty(),
            "nothing reaches the wire when materialization times out"
        );
        let entries = entries(&state);
        let entry = entry_for(&entries, PATH_A);
        assert_eq!(entry.outcome, SUPPLY_OUTCOME_FAILED);
        let detail = entry.detail.clone().unwrap_or_default();
        assert!(
            detail.contains("response headers"),
            "the failure detail names the header-phase wait: {detail:?}"
        );
        server.abort();
    }

    /// The breaker stops transport calls, and what it skips is BOOKKEEPING,
    /// not settlement: settled `failed` rows exist only for paths with an
    /// actual claim-resolved transport attempt (the supply rollup retires
    /// dependents from settled rows only, and the wired per-submission
    /// top-up's documented one-more-attempt covers exactly the paths the
    /// prewarm pass skipped — see `LadderTopup`). Pre-trip paths really
    /// were attempted and settle `failed`; post-trip paths are recorded
    /// `skipped` with the gateway detail and their claims are released so
    /// a later top-up can re-claim them.
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
        // remaining six planned paths are skipped without any further
        // transport calls.
        assert_eq!(fake.upload_calls.load(Ordering::SeqCst), 6);
        assert_eq!(report.failed, 6);
        assert_eq!(report.skipped, total_items - 6);
        assert_eq!(report.delivered, 0);
        let entries = entries(&state);
        for path in &paths[..6] {
            let entry = entry_for(&entries, path);
            assert_eq!(entry.outcome, SUPPLY_OUTCOME_FAILED, "{path}");
            assert_ne!(entry.detail.as_deref(), Some(GATEWAY_UNREACHABLE));
        }
        for path in &paths[6..] {
            let entry = entry_for(&entries, path);
            assert_eq!(entry.outcome, SUPPLY_OUTCOME_SKIPPED, "{path}");
            assert_eq!(entry.detail.as_deref(), Some(GATEWAY_UNREACHABLE));
            // The claim was won and released, not leaked: a later top-up
            // (fresh breaker) can claim the path again.
            assert_eq!(claims.claim(path), ClaimOutcome::Won, "{path}");
        }
        // The collapse is reported for the stage's operator PAUSE gate.
        assert!(report.upload_collapsed);
    }

    /// The upload-collapse PAUSE gate, both directions, on the real stage
    /// surface: a prewarm pass whose circuit breaker trips writes the
    /// campaign PAUSE file naming the collapse (the operator decides —
    /// proceed on the per-submission top-up alone, re-run the stage, or
    /// abort — exactly like the sibling prefetch-shortfall gate), while a
    /// healthy prewarm pass of the same shape pauses nothing. The wide
    /// archive's units are independent, so every drv text dials the
    /// gateway and the scripted transport failures latch the breaker.
    #[tokio::test]
    async fn run_supply_stage_pauses_when_the_upload_breaker_collapses() {
        use crate::run::archive_input::write_mini_wide_archive;

        let inputs_for = |archive: Arc<ReplayArchive>, drvs: &[String]| SupplyInputs {
            workload_outputs: BTreeSet::new(),
            workload_drvs: drvs.iter().cloned().collect(),
            prefetch_paths: BTreeMap::new(),
            prior_valid: BTreeSet::new(),
            target_coverage: BTreeSet::new(),
            archive: Some(archive),
            target_substituters: Vec::new(),
            relay_substituters: Vec::new(),
            // Hermetic: no substituter rungs, so the test never probes.
            dependencies: SupplyDependencies::EmbeddedOnly,
            delivery: SupplyDelivery::Prewarm,
        };
        let knobs = Knobs {
            upload_workers: 1,
            upload_batch_max_entries: 1,
            ..Knobs::default()
        };

        // Collapse: every upload fails; eight independent drv texts give
        // eight consecutive transport failures (threshold 6).
        let archive_dir = tempfile::tempdir().unwrap();
        let drvs = write_mini_wide_archive(archive_dir.path(), 8);
        let archive = Arc::new(ReplayArchive::open(archive_dir.path()).unwrap());
        let (_dir, state) = state();
        let state = Arc::new(state);
        let fake = Arc::new(FakeSupplyTransport::default());
        fake.fail_uploads.store(true, Ordering::SeqCst);
        let report = run_supply_stage(
            state.clone(),
            fake,
            inputs_for(archive.clone(), &drvs),
            &knobs,
            Arc::new(AtomicU64::new(1)),
            // Tests never block on the operator: the PAUSE file is
            // asserted on directly instead of being waited for.
            false,
        )
        .await
        .unwrap()
        .report;
        assert!(report.upload_collapsed);
        assert!(report.skipped > 0, "{report:?}");
        assert_eq!(
            std::fs::read_to_string(state.path("PAUSE")).unwrap(),
            "supply upload collapse\n",
            "the collapse must pause the campaign before execution"
        );

        // Healthy: same archive shape, accepting transport — no PAUSE.
        let archive_dir2 = tempfile::tempdir().unwrap();
        let drvs2 = write_mini_wide_archive(archive_dir2.path(), 8);
        let archive2 = Arc::new(ReplayArchive::open(archive_dir2.path()).unwrap());
        let (_dir2, state2) = state2();
        let state2 = Arc::new(state2);
        let report = run_supply_stage(
            state2.clone(),
            Arc::new(FakeSupplyTransport::default()),
            inputs_for(archive2, &drvs2),
            &knobs,
            Arc::new(AtomicU64::new(1)),
            false,
        )
        .await
        .unwrap()
        .report;
        assert!(!report.upload_collapsed);
        assert_eq!(report.delivered, 8, "{report:?}");
        assert!(
            !state2.path("PAUSE").exists(),
            "a healthy prewarm pass must not pause"
        );
    }

    /// One planned upload item per [`UploadPayload`] family. The `match`
    /// below is the totality tripwire: a new payload variant fails to
    /// compile here until this corpus (and therefore every test deriving
    /// its path vocabulary from it) covers the new family — the journal's
    /// producer vocabulary is enumerated from the producing enum, never
    /// hand-copied.
    fn item_per_payload_family(tag: &str) -> Vec<UploadItem> {
        let path = |family: &str| format!("/nix/store/{:0>32}-{tag}-{family}", family.len());
        let nar = vec![7u8; 16];
        let drv_text = item(&path("drvtext"), nar.clone(), &[]);
        let mut archive_path = item(&path("archive"), nar.clone(), &[]);
        archive_path.payload = UploadPayload::ArchivePath;
        let relay_path = path("relay");
        let mut relay = item(&relay_path, nar, &[]);
        relay.payload = UploadPayload::Relay {
            substituter_url: "https://cache.example.org".into(),
            narinfo: rio_nix::narinfo::NarInfo {
                store_path: relay_path,
                url: "nar/x.nar.zst".into(),
                compression: "zstd".into(),
                nar_hash: "sha256:0000000000000000000000000000000000000000000000000000".into(),
                nar_size: 16,
                references: Vec::new(),
                deriver: None,
                sigs: Vec::new(),
                ca: None,
                file_hash: None,
                file_size: None,
            },
        };
        let all = vec![drv_text, archive_path, relay];
        for member in &all {
            match &member.payload {
                UploadPayload::DrvText(_)
                | UploadPayload::ArchivePath
                | UploadPayload::Relay { .. } => {}
            }
        }
        all
    }

    /// The upload arms can never contradict the cross-request claims table,
    /// whatever the breaker state: settled rows (`delivered`/`refused`/
    /// `failed`) are minted only through claim resolution, landed paths get
    /// no row at all, and skips (breaker open, claim held) record `skipped`
    /// bookkeeping rows.
    ///
    /// Quantification domain: every (arm × claim-state × settlement-flavor
    /// × breaker-state) cell — arm ∈ {stream, batch} from the two upload
    /// entry points, claim-state ∈ {free, done-elsewhere, held-elsewhere}
    /// from [`ClaimOutcome`]'s three variants, settlement-flavor for the
    /// claim-free corpus ∈ {delivers, refused-by-daemon, transport-failed},
    /// breaker ∈ {closed, open} — with the path corpus of each cell
    /// spanning every [`UploadPayload`] family (via
    /// [`item_per_payload_family`]'s compile-time tripwire). The binding
    /// cross-component invariant of the supply rollup ("the journal's last
    /// settlement per path is its truth") is asserted over the union
    /// journal: no settled refused/failed row may exist for any path the
    /// claims table says landed.
    ///
    /// Release-ordering axis: every settled refused/failed row is appended
    /// while its claim is still held — [`record_settlement`] asserts it at
    /// the chokepoint, and every refused/failed cell here drives that
    /// assert — and the claim is released afterwards (re-claimable below),
    /// so there is no window in which a sibling can win the claim while
    /// the failure is unrecorded, and no leaked claim either.
    #[tokio::test]
    async fn upload_arms_never_contradict_the_claims_table() {
        for arm in ["stream", "batch"] {
            for breaker_open in [false, true] {
                let (_dir, state) = state();
                let fake = FakeSupplyTransport::default();
                let ctx = SupplyContext::new(SupplyDependencies::Substituters);
                let claims = UploadClaims::new();
                // claim_wait_mins = 0 keeps the held-claim bounded wait
                // instant: wait expires, the single re-claim still loses,
                // and the arm proceeds without the path (the claims
                // contract: claim / bounded wait / re-claim once / proceed).
                let knobs = Knobs {
                    upload_workers: 1,
                    upload_batch_max_entries: 1,
                    claim_wait_mins: 0,
                    ..Knobs::default()
                };

                // Trip the breaker (when this cell wants it open) with real
                // transport failures on sacrificial items, so the arms under
                // test run against a genuinely latched breaker.
                let trip_items: Vec<UploadItem> = if breaker_open {
                    fake.fail_uploads.store(true, Ordering::SeqCst);
                    (0..6)
                        .map(|index| {
                            item(
                                &format!("/nix/store/{:0>32}-trip-{index}", index),
                                vec![1u8; 8],
                                &[],
                            )
                        })
                        .collect()
                } else {
                    Vec::new()
                };

                // Cell corpus: one item per payload family per claim state,
                // plus the two settled-undelivered flavors of the claim-free
                // state. Only the DrvText member of those reaches the wire
                // (the relay member fails at materialization — no admitted
                // substituter — and the archive member has no archive
                // here), so the daemon-refusal/transport-failure flavor is
                // pinned on the DrvText member and the others settle as
                // materialization failures.
                let free = item_per_payload_family("free");
                let refused_corpus = item_per_payload_family("refused");
                let failed_corpus = item_per_payload_family("failed");
                let done = item_per_payload_family("done");
                let held = item_per_payload_family("held");
                {
                    // Two refusals per path: the first attempt and the one
                    // fresh-channel retry both refused → a settled
                    // `refused` row.
                    let mut refusals = fake.refusals.lock().unwrap();
                    for member in &refused_corpus {
                        refusals.insert(member.store_path.clone(), 2);
                    }
                    let mut fail_paths = fake.fail_paths.lock().unwrap();
                    for member in &failed_corpus {
                        fail_paths.insert(member.store_path.clone());
                    }
                }
                // A sibling request already delivered the `done` paths and
                // still holds the `held` paths.
                for member in &done {
                    assert_eq!(claims.claim(&member.store_path), ClaimOutcome::Won);
                    claims.complete(&member.store_path);
                }
                for member in &held {
                    assert_eq!(claims.claim(&member.store_path), ClaimOutcome::Won);
                }

                let mut all: Vec<UploadItem> = trip_items;
                all.extend(free.iter().cloned());
                all.extend(refused_corpus.iter().cloned());
                all.extend(failed_corpus.iter().cloned());
                all.extend(done.iter().cloned());
                all.extend(held.iter().cloned());
                let plan = match arm {
                    "stream" => UploadPlan {
                        large: all,
                        batch: Vec::new(),
                        skipped: Vec::new(),
                    },
                    _ => batch_plan(all),
                };
                prewarm_uploads(&fake, None, &ctx, &plan, &knobs, &state, &claims)
                    .await
                    .unwrap();

                let journal = entries(&state);
                let cell = format!("arm={arm} breaker_open={breaker_open}");

                // Landed-elsewhere paths: NO row of any kind, and the
                // validity set learned them.
                let valid = ctx.target_valid.read().await.clone();
                for member in &done {
                    assert!(
                        !journal.iter().any(|e| e.path == member.store_path),
                        "{cell}: landed path must get no row: {journal:?}"
                    );
                    assert!(valid.contains(&member.store_path), "{cell}");
                }
                // Held-elsewhere paths: exactly one `skipped` bookkeeping
                // row naming the held claim — never a settlement.
                for member in &held {
                    let rows: Vec<&SupplyEntry> = journal
                        .iter()
                        .filter(|e| e.path == member.store_path)
                        .collect();
                    assert_eq!(rows.len(), 1, "{cell}: {rows:?}");
                    assert_eq!(rows[0].outcome, SUPPLY_OUTCOME_SKIPPED, "{cell}");
                    assert_eq!(rows[0].detail.as_deref(), Some(CLAIM_STILL_HELD), "{cell}");
                }
                // Free paths: with the breaker open they are skipped without
                // an attempt (and their claims released for a later top-up);
                // with it closed they settle through a real attempt.
                for member in &free {
                    let row = entry_for(&journal, &member.store_path);
                    if breaker_open {
                        assert_eq!(row.outcome, SUPPLY_OUTCOME_SKIPPED, "{cell}");
                        assert_eq!(row.detail.as_deref(), Some(GATEWAY_UNREACHABLE), "{cell}");
                        assert_eq!(
                            claims.claim(&member.store_path),
                            ClaimOutcome::Won,
                            "{cell}"
                        );
                    } else {
                        assert!(
                            supply_outcome_is_settlement(&row.outcome),
                            "{cell}: a claim-won attempt must settle: {row:?}"
                        );
                    }
                }
                // Settled-undelivered flavors (claim-free corpus): under a
                // closed breaker every member settles refused/failed, with
                // the wire flavor pinned on the DrvText member; the row was
                // appended while the claim was held (record_settlement's
                // assert ran in this very cell) and the claim is released
                // AFTERWARDS — re-claimable now, so the failure is durable
                // before any sibling can win the path, and nothing leaks.
                // Under an open breaker they are skipped like any free path.
                for (corpus, wire_outcome) in [
                    (&refused_corpus, SUPPLY_OUTCOME_REFUSED),
                    (&failed_corpus, SUPPLY_OUTCOME_FAILED),
                ] {
                    for member in corpus {
                        let row = entry_for(&journal, &member.store_path);
                        if breaker_open {
                            assert_eq!(row.outcome, SUPPLY_OUTCOME_SKIPPED, "{cell}");
                        } else {
                            assert!(
                                row.outcome == SUPPLY_OUTCOME_REFUSED
                                    || row.outcome == SUPPLY_OUTCOME_FAILED,
                                "{cell}: must settle undelivered: {row:?}"
                            );
                            if matches!(member.payload, UploadPayload::DrvText(_)) {
                                assert_eq!(row.outcome, wire_outcome, "{cell}: {row:?}");
                            }
                        }
                        assert_eq!(
                            claims.claim(&member.store_path),
                            ClaimOutcome::Won,
                            "{cell}: the settled-undelivered claim must be released \
                             (after the row), never leaked"
                        );
                    }
                }

                // The binding invariant, over the whole cell journal: no
                // settled undelivered row contradicts a landed path, and
                // `skipped` is never a settlement.
                let landed: BTreeSet<&str> = done.iter().map(|m| m.store_path.as_str()).collect();
                for row in &journal {
                    if row.outcome == SUPPLY_OUTCOME_REFUSED || row.outcome == SUPPLY_OUTCOME_FAILED
                    {
                        assert!(
                            !landed.contains(row.path.as_str()) && !valid.contains(&row.path),
                            "{cell}: settled undelivered row for a landed path: {row:?}"
                        );
                    }
                }
            }
        }
    }

    /// The failed-reference pre-skip arms (stream and batch) settle the
    /// dependent `failed` naming the culprit reference, under the same
    /// resolution/append ordering as every other settled-undelivered row:
    /// the row is appended while the dependent's claim is held
    /// ([`record_settlement`]'s assert runs on these arms too) and the
    /// claim is released afterwards — re-claimable, never leaked. These
    /// two sites are the only settled-row mints the claims lattice's
    /// reference-free corpus cannot reach.
    #[tokio::test]
    async fn failed_reference_arms_settle_dependents_after_recording() {
        for arm in ["stream", "batch"] {
            let (_dir, state) = state();
            let fake = FakeSupplyTransport::default();
            let ctx = SupplyContext::new(SupplyDependencies::Substituters);
            let claims = UploadClaims::new();
            let knobs = Knobs {
                upload_workers: 1,
                upload_batch_max_entries: 1,
                claim_wait_mins: 0,
                ..Knobs::default()
            };
            let culprit = format!("/nix/store/{:0>32}-culprit", 1);
            let dependent = format!("/nix/store/{:0>32}-dependent", 2);
            fake.fail_paths.lock().unwrap().insert(culprit.clone());
            let items = vec![
                item(&culprit, vec![3u8; 8], &[]),
                item(&dependent, vec![4u8; 8], &[culprit.as_str()]),
            ];
            let plan = match arm {
                "stream" => UploadPlan {
                    large: items,
                    batch: Vec::new(),
                    skipped: Vec::new(),
                },
                _ => batch_plan(items),
            };
            prewarm_uploads(&fake, None, &ctx, &plan, &knobs, &state, &claims)
                .await
                .unwrap();
            let journal = entries(&state);
            assert_eq!(
                entry_for(&journal, &culprit).outcome,
                SUPPLY_OUTCOME_FAILED,
                "{arm}: {journal:?}"
            );
            let row = entry_for(&journal, &dependent);
            assert_eq!(row.outcome, SUPPLY_OUTCOME_FAILED, "{arm}: {journal:?}");
            assert!(
                row.detail.as_deref().unwrap_or_default().contains(&culprit),
                "{arm}: the dependent's row must name the failed reference: {row:?}"
            );
            for path in [&culprit, &dependent] {
                assert_eq!(
                    claims.claim(path),
                    ClaimOutcome::Won,
                    "{arm}: settled-undelivered claims are released after their rows"
                );
            }
        }
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

    /// A prefetch build that answers fewer results than submitted roots
    /// degrades per contract: the uncovered root's paths are recorded
    /// failed by the missing-result rule (with an explanatory detail) while
    /// covered roots keep their dispositions — no outcome is invented and
    /// nothing panics.
    #[tokio::test]
    async fn prefetch_arm_marks_roots_without_results_failed() {
        let (_dir, state) = state();
        let fake = FakeSupplyTransport::default();
        let drv_a = "/nix/store/cccccccccccccccccccccccccccccccc-a.drv";
        let drv_b = "/nix/store/dddddddddddddddddddddddddddddddd-b.drv";
        let path_a = "/nix/store/ffffffffffffffffffffffffffffffff-out-a";
        let path_b = "/nix/store/gggggggggggggggggggggggggggggggg-out-b";
        fake.prefetch_omitted
            .lock()
            .unwrap()
            .insert(drv_b.to_string());
        let prefetch_roots: BTreeMap<String, Vec<String>> = [
            (drv_a.to_string(), vec![path_a.to_string()]),
            (drv_b.to_string(), vec![path_b.to_string()]),
        ]
        .into();
        let batch_seq = AtomicU64::new(1);

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
        assert_eq!(entry_a.outcome, SUPPLY_OUTCOME_DELEGATED);
        assert_eq!(entry_b.outcome, SUPPLY_OUTCOME_FAILED);
        assert_eq!(
            entry_b.detail.as_deref(),
            Some("the prefetch build returned no result for this root")
        );
        assert_eq!(stats.delegated, 1);
        assert_eq!(stats.failed, 1);
    }

    /// `prefetch_build` correlates the daemon's results with the submitted
    /// roots through the shared positional mapping, so a result count
    /// differing from the roots warns (the contract-mandated length check)
    /// instead of zipping silently — pinned here because the prefetch arm
    /// is the call site most likely to face a non-rio daemon.
    #[test]
    #[tracing_test::traced_test]
    fn prefetch_result_mapping_warns_on_count_mismatch() {
        let roots = vec![
            "/nix/store/cccccccccccccccccccccccccccccccc-a.drv".to_string(),
            "/nix/store/dddddddddddddddddddddddddddddddd-b.drv".to_string(),
        ];
        let keyed = vec![KeyedBuildResult {
            derived_path: format!("{}!*", roots[0]),
            result: BuildResult {
                status: BuildStatus::Substituted,
                ..BuildResult::default()
            },
        }];

        let outcomes = path_outcomes_from_keyed(&roots, &keyed);

        assert_eq!(outcomes.len(), 1);
        assert_eq!(outcomes[0].drv_path, roots[0]);
        assert_eq!(
            outcomes[0].status,
            build_status_name(BuildStatus::Substituted)
        );
        assert!(logs_contain(
            "BuildPathsWithResults returned a different result count than requested roots"
        ));
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

    /// A supply-report.json artifact written before the `skipped` count
    /// and the `upload_collapsed` flag existed still parses: both default
    /// (0 / false — an old report never claims a collapse), and the
    /// struct tolerates unknown keys so a rolled-back reader survives a
    /// newer artifact too.
    #[test]
    fn supply_report_artifacts_from_before_the_skip_vocabulary_parse() {
        let old = r#"{"plannedPrefetch":4,"prefetchMissing":1,"prefetchUnavailable":0,
            "delivered":2,"delegated":1,"alreadyPresent":0,"refused":1,"unavailable":0,
            "failed":1,"uploadedBytes":1024,"uploadSecs":2.0,"uploadMibPerS":null,
            "probeErrors":{},"shortfallPct":25.0}"#;
        let parsed: SupplyStageReport = serde_json::from_str(old).unwrap();
        assert_eq!(parsed.skipped, 0);
        assert!(!parsed.upload_collapsed);
        assert_eq!(parsed.delivered, 2);
        // Rollback direction: a NEWER artifact (unknown future key) is
        // readable by this version — unknown keys are ignored.
        let newer = r#"{"delivered":1,"someFutureKey":true}"#;
        let parsed: SupplyStageReport = serde_json::from_str(newer).unwrap();
        assert_eq!(parsed.delivered, 1);
    }

    /// Bookkeeping rows never displace settlements in the outcome fold —
    /// in BOTH directions of the must-not-shadow contract: a `skipped` row
    /// appended after the claim holder's `delivered` leaves the path
    /// counted delivered (a skip-held racer must not erase a delivery),
    /// and one appended after a real `failed` leaves it counted failed (a
    /// later breaker skip must not launder a genuine failure). Settlements
    /// still supersede anything earlier, bookkeeping supersedes only
    /// bookkeeping, and a path with nothing but bookkeeping keeps its
    /// bookkeeping count.
    ///
    /// Quantification domain: the outcome split is [`SUPPLY_OUTCOMES`] via
    /// `supply_outcome_is_settlement` (the closed vocabulary as data) —
    /// the final sweep drives one path per outcome through the fold so a
    /// new outcome constant cannot ship without a counted (or explicitly
    /// bookkeeping) disposition here.
    #[test]
    fn journal_outcome_counts_let_settlements_beat_bookkeeping() {
        let entry = |path: &str, outcome: &str| SupplyEntry {
            path: path.into(),
            source: SUPPLY_SOURCE_EMBEDDED.into(),
            mechanism: SUPPLY_MECHANISM_UPLOAD_BATCH.into(),
            outcome: outcome.into(),
            detail: None,
            batch_id: None,
            bytes: None,
            observed_at: now_rfc3339(),
        };
        const PATH_C: &str = "/nix/store/cccccccccccccccccccccccccccccccc-supply-c";
        const PATH_D: &str = "/nix/store/dddddddddddddddddddddddddddddddd-supply-d";
        let entries = vec![
            // Holder delivered, then a sibling's skip-held row landed late.
            entry(PATH_A, SUPPLY_OUTCOME_DELIVERED),
            entry(PATH_A, SUPPLY_OUTCOME_SKIPPED),
            // Real failure, then a breaker skip on a later invocation.
            entry(PATH_B, SUPPLY_OUTCOME_FAILED),
            entry(PATH_B, SUPPLY_OUTCOME_SKIPPED),
            // Bookkeeping only: deferred, then skipped — counted skipped.
            entry(PATH_C, SUPPLY_OUTCOME_UNAVAILABLE),
            entry(PATH_C, SUPPLY_OUTCOME_SKIPPED),
            // Skipped first, then a real attempt delivered it.
            entry(PATH_D, SUPPLY_OUTCOME_SKIPPED),
            entry(PATH_D, SUPPLY_OUTCOME_DELIVERED),
        ];
        let mut report = SupplyStageReport::default();
        refresh_outcome_counts(&mut report, &entries);
        assert_eq!(report.delivered, 2, "{report:?}");
        assert_eq!(report.failed, 1, "{report:?}");
        assert_eq!(report.skipped, 1, "{report:?}");
        assert_eq!(report.unavailable, 0, "{report:?}");

        // Vocabulary sweep: one path per outcome — every outcome in the
        // closed set lands in exactly the count its settlement class says.
        let per_outcome: Vec<SupplyEntry> = crate::run::model::SUPPLY_OUTCOMES
            .iter()
            .enumerate()
            .map(|(index, outcome)| entry(&format!("/nix/store/{index:0>32}-sweep"), outcome))
            .collect();
        let mut sweep = SupplyStageReport::default();
        refresh_outcome_counts(&mut sweep, &per_outcome);
        assert_eq!(
            (
                sweep.delivered,
                sweep.already_present,
                sweep.delegated,
                sweep.refused,
                sweep.unavailable,
                sweep.failed,
                sweep.skipped
            ),
            (1, 1, 1, 1, 1, 1, 1),
            "{sweep:?}"
        );
    }

    #[test]
    fn tenant_key_path_joins_only_plain_file_names() {
        // The tenant name is joined onto the key directory as a path
        // component; traversal sequences, separators, and empty names must
        // never reach that join.
        assert_eq!(
            tenant_key_path(std::path::Path::new("/etc/rio/replay-ssh"), "replay-warm").unwrap(),
            std::path::PathBuf::from("/etc/rio/replay-ssh/replay-warm")
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
        // One extra path already valid at the plan snapshot (bookkeeping
        // only — never part of the wanted-set arithmetic) and one covered
        // path with no producing derivation: the latter cannot be planned
        // for prefetch, so it joins the shortfall denominators as
        // unavailable instead of silently shrinking what the gate measures.
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
        .unwrap()
        .report;

        assert_eq!(report.planned_prefetch, 10);
        assert_eq!(report.prefetch_missing, 2);
        assert_eq!(report.prefetch_unavailable, 1);
        // (2 missing + 1 unavailable) / (10 planned + 1 unavailable).
        let expected_pct = 3.0 / 11.0 * 100.0;
        assert!(
            (report.shortfall_pct.unwrap() - expected_pct).abs() < 1e-9,
            "{:?}",
            report.shortfall_pct
        );
        assert_eq!(report.delegated, 8);
        assert_eq!(report.already_present, 1);
        assert_eq!(report.unavailable, 1);
        assert!(
            state.path("PAUSE").exists(),
            "a shortfall above the 10% threshold must create the PAUSE file"
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
        .unwrap()
        .report;
        assert_eq!(report.prefetch_missing, 0);
        assert_eq!(report.shortfall_pct, Some(0.0));
        assert!(
            !state2.path("PAUSE").exists(),
            "no shortfall, no PAUSE file"
        );
    }

    /// The durability scenario the gate exists for: every prefetch-wanted
    /// path is unavailable (not coverable upstream, or covered with no
    /// producing derivation), so NOTHING can be planned for prefetch. The
    /// shortfall must read 100% — pausing the campaign before its
    /// execution clock starts — instead of the planned-set guard skipping
    /// the gate, leaving shortfall_pct None, and starting a silently
    /// under-supplied measurement no low-confidence flag can reach.
    #[tokio::test]
    async fn run_supply_stage_pauses_when_the_whole_wanted_set_is_unavailable() {
        let (_dir, state) = state();
        let state = Arc::new(state);
        let fake = Arc::new(FakeSupplyTransport::default());
        let uncovered: Vec<String> = (0..3)
            .map(|i| format!("/nix/store/{:0>32}-uncovered{i}", i))
            .collect();
        let no_producer = "/nix/store/qqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqq-noproducer".to_string();
        let mut inputs = prefetch_only_inputs(&[]);
        for path in &uncovered {
            // Producer known, but no upstream coverage and no ladder
            // source: unprefetchable and undeliverable.
            inputs
                .prefetch_paths
                .insert(path.clone(), Some(producing_drv(path)));
        }
        inputs.prefetch_paths.insert(no_producer.clone(), None);
        inputs.target_coverage.insert(no_producer.clone());
        let knobs = Knobs {
            prefetch_shortfall_pause_pct: 10.0,
            ..Knobs::default()
        };
        let report = run_supply_stage(
            state.clone(),
            fake,
            inputs,
            &knobs,
            Arc::new(AtomicU64::new(1)),
            false,
        )
        .await
        .unwrap()
        .report;
        assert_eq!(report.planned_prefetch, 0, "{report:?}");
        assert_eq!(report.prefetch_unavailable, 4);
        assert_eq!(report.shortfall_pct, Some(100.0));
        assert!(
            state.path("PAUSE").exists(),
            "an all-unavailable wanted set must pause, not skip the gate"
        );
        // The low-confidence flag derives from shortfall_pct > 0, so the
        // report side can also see the degraded measurement.
        assert!(report.shortfall_pct.is_some_and(|pct| pct > 0.0));
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

        let output = run_supply_stage(
            state.clone(),
            fake.clone(),
            inputs,
            &Knobs::default(),
            Arc::new(AtomicU64::new(1)),
            false,
        )
        .await
        .unwrap();
        let report = output.report;
        assert!(
            output.ladder.is_some(),
            "the upload ladder ran, so its context is handed back for top-ups"
        );

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

    /// Open the mini archive and return it together with the appB workload
    /// unit (the unit whose closure carries the libA and stdenv derivations).
    fn mini_archive_app_b() -> (
        tempfile::TempDir,
        Arc<crate::archive::reader::ReplayArchive>,
        crate::run::archive_input::ManifestEntry,
    ) {
        let archive_dir = tempfile::tempdir().unwrap();
        crate::run::archive_input::write_mini_archive(archive_dir.path());
        let archive =
            Arc::new(crate::archive::reader::ReplayArchive::open(archive_dir.path()).unwrap());
        let app_b = crate::run::archive_input::load_units(&archive)
            .unwrap()
            .into_iter()
            .find(|unit| unit.job == "appB.x86_64-linux")
            .unwrap();
        (archive_dir, archive, app_b)
    }

    /// The production pre-submission hook delivers what the prewarm pass
    /// missed, records it, and never re-uploads what an earlier call already
    /// delivered.
    #[tokio::test]
    async fn ladder_topup_delivers_prewarm_missed_paths_only_once() {
        let (_archive_dir, archive, app_b) = mini_archive_app_b();
        let (_dir, state) = state();
        let state = Arc::new(state);
        let fake = Arc::new(FakeSupplyTransport::default());
        let mut ctx = SupplyContext::new(SupplyDependencies::Substituters);
        ctx.workload_outputs = app_b.outputs.values().cloned().collect();
        let topup = LadderTopup::new(
            fake.clone(),
            archive.clone(),
            ctx,
            Knobs::default(),
            state.clone(),
        );
        let roots = vec![app_b.drv_path.clone()];

        // Nothing is valid on the target (the prewarm pass missed the whole
        // closure): the top-up delivers the three derivation texts
        // (appB → libA → stdenv) and records each as a supply entry.
        topup.topup(&roots).await.unwrap();
        let uploaded: BTreeSet<String> = fake
            .uploaded_batches
            .lock()
            .unwrap()
            .iter()
            .flatten()
            .cloned()
            .collect();
        assert_eq!(uploaded.len(), 3, "{uploaded:?}");
        assert!(uploaded.contains(&app_b.drv_path));
        let journal = entries(&state);
        for path in &uploaded {
            assert_eq!(
                entry_for(&journal, path).outcome,
                SUPPLY_OUTCOME_DELIVERED,
                "{journal:?}"
            );
        }
        let upload_calls = fake.upload_calls.load(Ordering::SeqCst);
        assert!(upload_calls > 0);

        // Everything the first call delivered is remembered by the shared
        // ladder context, so a second top-up for the same roots makes no
        // further upload calls and appends nothing.
        topup.topup(&roots).await.unwrap();
        assert_eq!(fake.upload_calls.load(Ordering::SeqCst), upload_calls);
        assert_eq!(entries(&state).len(), journal.len());
    }

    /// A top-up over a target that already has the whole closure makes no
    /// upload calls at all (the validity probe is the only traffic).
    #[tokio::test]
    async fn ladder_topup_makes_no_upload_calls_when_nothing_is_missing() {
        let (_archive_dir, archive, app_b) = mini_archive_app_b();
        let (_dir, state) = state();
        let state = Arc::new(state);
        let fake = Arc::new(FakeSupplyTransport::default());
        // The target already holds every closure member (delivered by the
        // prewarm pass, or valid all along).
        let closure = walk_closure(&archive, std::slice::from_ref(&app_b.drv_path)).unwrap();
        fake.valid
            .lock()
            .unwrap()
            .extend(closure.all_paths.iter().cloned());
        let mut ctx = SupplyContext::new(SupplyDependencies::Substituters);
        ctx.workload_outputs = app_b.outputs.values().cloned().collect();
        let topup = LadderTopup::new(
            fake.clone(),
            archive.clone(),
            ctx,
            Knobs::default(),
            state.clone(),
        );

        topup
            .topup(std::slice::from_ref(&app_b.drv_path))
            .await
            .unwrap();

        assert_eq!(fake.upload_calls.load(Ordering::SeqCst), 0);
        assert!(
            entries(&state).is_empty(),
            "nothing was missing, so nothing is recorded"
        );
    }
}
