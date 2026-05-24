//! Pre-warm phase for `xtask k8s replay` — the bulk supply pass that runs
//! before the replay clock starts.
//!
//! Prewarm exists for timing fidelity: every dependency upload that can be
//! done up front is done up front, so request latencies measured during the
//! timeline reflect builds, not uploads. The work splits into two halves:
//!
//! - [`build_supply_context`] computes everything the supply ladder needs
//!   once per run WITHOUT touching the target: the union closure of all
//!   recorded requests, the workload set and its output paths, which paths
//!   the target's own substituters cover, and which paths must be relayed
//!   from the recording's substituters. The `--no-prewarm` and `--dry-run`
//!   paths reuse it unchanged.
//! - [`run`] is the upload half: probe what the target already has, plan the
//!   uploads ([`plan_uploads`]), and push everything supplyable in
//!   reference-safe order (large relayed paths streamed individually first,
//!   then the batch in topological levels over a small worker pool).
//!
//! Failure stance: per-path and per-batch problems degrade — they are
//! recorded in the [`PrewarmReport`] and the affected paths are left for the
//! per-request fallback inside the timeline. Only systemic failures (the
//! target cannot be reached at all) abort the run.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use futures_util::StreamExt as _;
use rio_nix::narinfo::NarInfo;
use rio_nix::protocol::client::{NarPayload, StoreEntry};

use super::archive::{ReplayArchive, hash_part};
use super::client::{DaemonChannel, GatewayPool};
use super::substituter::Substituter;
use super::supply::{
    Closure, PathSource, UploadItem, UploadPayload, WorkloadSet, plan_uploads, resolve_source,
    walk_closure, workload_set,
};
use crate::ui;

/// Per-operation deadline for individually streamed (large) relay uploads.
/// Probes and batch uploads use [`PrewarmConfig::op_timeout`]; a multi-GB NAR
/// legitimately needs longer.
const LARGE_UPLOAD_TIMEOUT: Duration = Duration::from_secs(600);

/// Emit a batch-upload progress line every this many completed sub-batches.
const PROGRESS_EVERY: usize = 25;

/// Emit a substituter-coverage progress line every this many probed paths.
const COVERAGE_PROGRESS_EVERY: usize = 5000;

/// Failure reason recorded for uploads abandoned after the dial circuit
/// breaker ([`GatewayBreaker`]) trips.
const GATEWAY_UNREACHABLE: &str = "gateway unreachable; not retried";

/// Everything the supply ladder needs that is computed once per run and
/// shared between prewarm and the per-request path.
pub struct SupplyContext {
    /// Derivations the target must build itself (never supplied).
    pub workload: WorkloadSet,
    /// Output paths produced by workload drvs (never supplied).
    pub workload_outputs: BTreeSet<String>,
    /// Paths covered by at least one `--target-substituter` (probe positives).
    pub target_coverage: BTreeSet<String>,
    /// Full store path → (substituter url, narinfo) for relay-able paths —
    /// keyed by the full path because [`resolve_source`] looks paths up that
    /// way.
    pub relay_narinfos: HashMap<String, (String, NarInfo)>,
    /// Paths known valid on the target (probed valid + successfully
    /// uploaded). Prewarm fills this; the timeline updates it as uploads
    /// land.
    pub target_valid: BTreeSet<String>,
    /// Per-substituter probe failure counts (cache url → errors) —
    /// informational.
    pub probe_errors: BTreeMap<String, u64>,
}

/// Tunables for the prewarm phase. [`Default`] matches the CLI defaults;
/// [`PrewarmConfig::for_pool`] derives `upload_workers` from an actual pool.
///
/// Memory note: every batch worker buffers one materialized sub-batch, so
/// the batch phase's peak resident set is roughly `upload_workers ×
/// batch_max_bytes` (large relayed paths are streamed and don't add to it).
#[derive(Debug, Clone)]
pub struct PrewarmConfig {
    /// Paths per `QueryValidPaths` probe.
    pub probe_chunk: usize,
    /// Concurrent daemon channels for the target validity probe.
    pub probe_concurrency: usize,
    /// Concurrent substituter narinfo probes (coverage + relay resolution).
    pub coverage_concurrency: usize,
    /// Concurrent batch-upload workers (one daemon channel each). Workers buy
    /// upload round-trip overlap, not bandwidth — and each buffers up to
    /// `batch_max_bytes`, so peak memory scales with `upload_workers ×
    /// batch_max_bytes`.
    pub upload_workers: usize,
    /// Byte budget (sum of NAR sizes) per batch upload. Every worker buffers
    /// one materialized batch, so peak memory is roughly `upload_workers ×`
    /// this.
    pub batch_max_bytes: u64,
    /// Entry budget per batch upload.
    pub batch_max_entries: usize,
    /// Deadline for probes, payload materialization, and batch uploads;
    /// individually streamed large paths get [`LARGE_UPLOAD_TIMEOUT`]
    /// instead.
    pub op_timeout: Duration,
}

impl Default for PrewarmConfig {
    fn default() -> Self {
        Self {
            probe_chunk: 2000,
            probe_concurrency: 3,
            coverage_concurrency: 16,
            // Workers buy round-trip overlap, not bytes in flight; 8 keeps
            // the worst-case buffered payload (8 × 256 MiB = 2 GiB) sane.
            upload_workers: 8,
            batch_max_bytes: 256 * 1024 * 1024,
            batch_max_entries: 500,
            op_timeout: Duration::from_secs(120),
        }
    }
}

impl PrewarmConfig {
    /// Defaults with `upload_workers` derived from the pool: half its channel
    /// capacity, at least 1 and at most 8 (more workers would only multiply
    /// peak memory, not throughput).
    pub fn for_pool(pool: &GatewayPool) -> Self {
        Self {
            upload_workers: (pool.capacity() / 2).clamp(1, 8),
            ..Self::default()
        }
    }
}

/// What prewarm did, for the run report and the console summary.
#[derive(Debug, Default, serde::Serialize)]
pub struct PrewarmReport {
    /// Paths in the union closure (drvs, sources, known outputs).
    pub union_paths: usize,
    /// Derivations in the union closure.
    pub union_drvs: usize,
    /// Paths the target already had before prewarm uploaded anything.
    pub already_valid: usize,
    /// Paths a `--target-substituter` covers (left for the target to fetch).
    pub target_substitutable: usize,
    /// Workload outputs withheld so the target builds them itself.
    pub workload_withheld: usize,
    /// Items the upload plan contained (large + batch).
    pub planned_uploads: usize,
    /// Paths successfully uploaded.
    pub uploaded_paths: usize,
    /// Total uncompressed NAR bytes successfully uploaded.
    pub uploaded_bytes: u64,
    /// `(path, reason)` pairs the planner could not place (from
    /// [`super::supply::UploadPlan::skipped`]).
    pub skipped: Vec<(String, String)>,
    /// `(path, error)` pairs for failed uploads — degraded, not fatal. A
    /// batch that fails mid-stream records every entry of that batch,
    /// including ones the daemon may already have ingested, so this can
    /// over-count.
    pub upload_failures: Vec<(String, String)>,
    /// `(path, error)` pairs for failed relay fetches — degraded, not fatal.
    pub relay_failures: Vec<(String, String)>,
    /// Per-substituter probe failure counts (cache url → errors).
    pub probe_errors: BTreeMap<String, u64>,
    /// Wall-clock duration of the upload half ([`run`]) only — building the
    /// supply context is not included.
    pub elapsed_secs: f64,
}

/// Build the run-wide supply context WITHOUT touching the target: union
/// closure of all requests (roots in the given order), workload set and
/// outputs, target-substituter coverage probes, and relay narinfo resolution
/// (against `src_substituters`, the recording's caches) for paths that are
/// neither covered nor embedded. Used by the prewarm, `--no-prewarm`, and
/// `--dry-run` paths alike — it never needs a [`GatewayPool`]; pass empty
/// substituter slices to skip all probing (e.g. for `--dry-run`).
pub async fn build_supply_context(
    archive: &Arc<ReplayArchive>,
    requests_roots: &[String],
    target_substituters: &[Substituter],
    src_substituters: &[Substituter],
    coverage_concurrency: usize,
) -> Result<(SupplyContext, Closure)> {
    // The archive crosses task boundaries here for the first time
    // (spawn_blocking + concurrent probes); fail compilation loudly if it
    // ever stops being shareable.
    fn _assert_send_sync<T: Send + Sync>() {}
    _assert_send_sync::<ReplayArchive>();

    let workload = workload_set(archive);

    // The closure walk reads and parses every derivation in the archive —
    // long, synchronous work that must not stall the async runtime.
    let closure = ui::step("replay: walk union closure", || {
        let archive = Arc::clone(archive);
        let roots = requests_roots.to_vec();
        async move {
            tokio::task::spawn_blocking(move || walk_closure(&archive, &roots))
                .await
                .context("the union closure walk task panicked or was cancelled")?
        }
    })
    .await?;

    // Outputs of workload drvs, taken from the closure's nodes — a workload
    // drv outside the closure cannot be requested, so these are sufficient.
    let workload_outputs: BTreeSet<String> = closure
        .topo
        .iter()
        .filter(|node| workload.drvs.contains(&node.drv_path))
        .flat_map(|node| node.outputs.values().filter(|path| !path.is_empty()))
        .cloned()
        .collect();

    let mut ctx = SupplyContext {
        workload,
        workload_outputs,
        target_coverage: BTreeSet::new(),
        relay_narinfos: HashMap::new(),
        target_valid: BTreeSet::new(),
        probe_errors: BTreeMap::new(),
    };

    // Substituter probes: one pass answers both questions ("can the target
    // fetch this itself?" and "which recording cache can relay it?").
    // Derivations are always uploaded as text and workload outputs are never
    // supplied, so neither needs a probe.
    let closure_drvs: BTreeSet<&str> = closure
        .topo
        .iter()
        .map(|node| node.drv_path.as_str())
        .collect();
    let probe_paths: Vec<(String, bool)> = closure
        .all_paths
        .iter()
        .filter(|path| !closure_drvs.contains(path.as_str()))
        .filter(|path| !ctx.workload_outputs.contains(*path))
        .map(|path| (path.clone(), archive.has_embedded(path)))
        .collect();

    // Nothing to probe (every path is a drv or workload output) or nowhere
    // to probe (no substituters configured, e.g. --dry-run): the coverage
    // and relay maps simply stay empty.
    if probe_paths.is_empty() || (target_substituters.is_empty() && src_substituters.is_empty()) {
        return Ok((ctx, closure));
    }

    let outcome = ui::step(
        &format!(
            "replay: probe substituter coverage ({} paths)",
            probe_paths.len()
        ),
        || async {
            Ok::<_, anyhow::Error>(
                probe_coverage(
                    probe_paths,
                    target_substituters,
                    src_substituters,
                    coverage_concurrency,
                )
                .await,
            )
        },
    )
    .await?;
    ctx.target_coverage = outcome.coverage;
    ctx.relay_narinfos = outcome.relay;
    ctx.probe_errors = outcome.errors;

    Ok((ctx, closure))
}

/// The bulk pre-supply phase: probe target validity for the union closure,
/// plan the uploads, materialize payloads, and upload everything supplyable
/// in reference-safe order before the replay clock starts.
///
/// Per-path and per-batch failures degrade (recorded in the report; the
/// affected paths are left for the per-request fallback) — they never abort
/// the run. Only systemic failures (no daemon channel can be opened at all
/// at the start of the validity probe) return an error.
pub async fn run(
    archive: &Arc<ReplayArchive>,
    pool: &GatewayPool,
    ctx: &mut SupplyContext,
    union_closure: &Closure,
    src_substituters: &[Substituter],
    cfg: &PrewarmConfig,
) -> Result<PrewarmReport> {
    let started = Instant::now();
    let mut report = PrewarmReport {
        union_paths: union_closure.all_paths.len(),
        union_drvs: union_closure.topo.len(),
        ..PrewarmReport::default()
    };

    // Phase 1: what does the target already have? Failed chunks degrade to
    // "not present" (we may upload more than necessary, which is safe).
    let probed_valid = ui::step(
        &format!(
            "prewarm: probe target validity ({} paths)",
            union_closure.all_paths.len()
        ),
        || probe_target_validity(pool, &union_closure.all_paths, cfg),
    )
    .await?;
    report.already_valid = probed_valid.len();
    ctx.target_valid.extend(probed_valid);

    // Phase 2: resolve a source for every non-derivation path the planner
    // could ask about. Derivation texts are always planned from the archive;
    // everything else MUST get an entry so plan_uploads never sees a
    // planner-input gap.
    let closure_drvs: BTreeSet<&str> = union_closure
        .topo
        .iter()
        .map(|node| node.drv_path.as_str())
        .collect();
    let mut sources: HashMap<String, PathSource> = HashMap::new();
    for path in &union_closure.all_paths {
        if closure_drvs.contains(path.as_str()) || ctx.target_valid.contains(path) {
            continue;
        }
        sources.insert(
            path.clone(),
            resolve_source(
                path,
                &ctx.workload_outputs,
                &ctx.target_coverage,
                archive,
                &ctx.relay_narinfos,
            ),
        );
    }
    report.target_substitutable = sources
        .values()
        .filter(|source| matches!(source, PathSource::TargetSubstituter))
        .count();
    report.workload_withheld = sources
        .values()
        .filter(|source| matches!(source, PathSource::NotSupplied { workload: true }))
        .count();

    // Phase 3: the reference-safe upload plan for the whole union closure.
    let plan = plan_uploads(union_closure, &sources, &ctx.target_valid, archive)?;
    report.planned_uploads = plan.large.len() + plan.batch.len();
    report.skipped = plan.skipped.clone();
    let planned_bytes: u64 = plan
        .large
        .iter()
        .chain(plan.batch.iter())
        .map(|item| item.info.nar_size)
        .sum();
    tracing::info!(
        already_valid = report.already_valid,
        target_substitutable = report.target_substitutable,
        workload_withheld = report.workload_withheld,
        planned_uploads = report.planned_uploads,
        planned_mib = planned_bytes / (1024 * 1024),
        skipped = report.skipped.len(),
        "prewarm upload plan ready"
    );

    // Look substituters up by the URL recorded in the relay narinfo map (the
    // same `Substituter::url()` string is stored there by the coverage pass).
    let substituters_by_url: HashMap<String, &Substituter> = src_substituters
        .iter()
        .map(|substituter| (substituter.url(), substituter))
        .collect();
    // Planned paths whose upload failed so far; their dependents are skipped
    // up front instead of being refused by the daemon one by one.
    let mut failed_paths: BTreeSet<String> = BTreeSet::new();
    // Stop dialing a gateway that is clearly gone instead of burning a
    // connect timeout on every remaining sub-batch.
    let breaker = GatewayBreaker::new(cfg.upload_workers.saturating_mul(2).max(6));

    // Phase 4: large relayed paths, streamed individually before the batch.
    if !plan.large.is_empty() {
        let outcome = ui::step(
            &format!("prewarm: stream {} large paths", plan.large.len()),
            || async {
                Ok::<_, anyhow::Error>(
                    upload_large(pool, &plan.large, &substituters_by_url, &breaker).await,
                )
            },
        )
        .await?;
        apply_outcome(outcome, ctx, &mut report, &mut failed_paths);
    }

    // Phase 5: the batch, level by level — level n+1 starts only after every
    // sub-batch of level n finished, so references are always present first.
    let levels = topo_levels(&plan.batch, &ctx.target_valid);
    for (level_index, level) in levels.iter().enumerate() {
        if level.is_empty() {
            continue;
        }
        let level_items: Vec<&UploadItem> = level.iter().map(|&index| &plan.batch[index]).collect();
        let level_bytes: u64 = level_items.iter().map(|item| item.info.nar_size).sum();
        let outcome = ui::step(
            &format!(
                "prewarm: upload level {level_index} ({} paths, {} MiB)",
                level_items.len(),
                level_bytes / (1024 * 1024),
            ),
            || async {
                Ok::<_, anyhow::Error>(
                    upload_level(
                        archive,
                        pool,
                        &level_items,
                        &substituters_by_url,
                        &failed_paths,
                        &breaker,
                        cfg,
                    )
                    .await,
                )
            },
        )
        .await?;
        apply_outcome(outcome, ctx, &mut report, &mut failed_paths);
    }

    report.probe_errors = ctx.probe_errors.clone();
    report.elapsed_secs = started.elapsed().as_secs_f64();
    tracing::info!(
        uploaded_paths = report.uploaded_paths,
        uploaded_mib = report.uploaded_bytes / (1024 * 1024),
        already_valid = report.already_valid,
        target_substitutable = report.target_substitutable,
        workload_withheld = report.workload_withheld,
        skipped = report.skipped.len(),
        upload_failures = report.upload_failures.len(),
        relay_failures = report.relay_failures.len(),
        elapsed_s = report.elapsed_secs,
        "prewarm finished"
    );
    Ok(report)
}

/// Group `batch` items into topological upload levels: an item's level is
/// `1 + max(level of its planned references)`, and items with no planned
/// references are level 0. A reference is "planned" when it names another
/// item of the batch and is not already valid on the target; self-references
/// never constrain. Returns indices into `batch` grouped by ascending level,
/// preserving batch order within a level.
///
/// `batch` is expected in [`plan_uploads`] order (references before
/// referrers). The one exception that order allows — a force-placed
/// derivation text whose reference appears later — is treated as
/// unconstraining here, mirroring the planner's own decision to place it.
pub fn topo_levels(batch: &[UploadItem], target_valid: &BTreeSet<String>) -> Vec<Vec<usize>> {
    let index_of: HashMap<&str, usize> = batch
        .iter()
        .enumerate()
        .map(|(index, item)| (item.store_path.as_str(), index))
        .collect();

    let mut level_of: Vec<usize> = vec![0; batch.len()];
    let mut levels: Vec<Vec<usize>> = Vec::new();
    for (index, item) in batch.iter().enumerate() {
        let mut level = 0;
        for reference in &item.info.references {
            if reference == &item.store_path || target_valid.contains(reference) {
                continue;
            }
            if let Some(&reference_index) = index_of.get(reference.as_str())
                && reference_index < index
            {
                level = level.max(level_of[reference_index] + 1);
            }
        }
        level_of[index] = level;
        if levels.len() <= level {
            levels.resize_with(level + 1, Vec::new);
        }
        levels[level].push(index);
    }
    levels
}

/// Split one level's items into sub-batches respecting both the byte cap
/// (sum of NAR sizes) and the entry cap, preserving order. An item larger
/// than `max_bytes` on its own gets its own sub-batch. Returns indices into
/// `items`.
pub fn split_batches(items: &[&UploadItem], max_bytes: u64, max_entries: usize) -> Vec<Vec<usize>> {
    let max_entries = max_entries.max(1);
    let mut batches: Vec<Vec<usize>> = Vec::new();
    let mut current: Vec<usize> = Vec::new();
    let mut current_bytes: u64 = 0;
    for (index, item) in items.iter().enumerate() {
        let size = item.info.nar_size;
        if size > max_bytes {
            if !current.is_empty() {
                batches.push(std::mem::take(&mut current));
                current_bytes = 0;
            }
            batches.push(vec![index]);
            continue;
        }
        if !current.is_empty() && (current.len() >= max_entries || current_bytes + size > max_bytes)
        {
            batches.push(std::mem::take(&mut current));
            current_bytes = 0;
        }
        current.push(index);
        current_bytes += size;
    }
    if !current.is_empty() {
        batches.push(current);
    }
    batches
}

/// What the substituter coverage pass learned about one path.
struct CoverageProbe {
    /// The probed store path.
    path: String,
    /// A target substituter has it.
    target_hit: bool,
    /// First recording substituter that has it (when relay is needed).
    relay: Option<(String, NarInfo)>,
    /// Probe errors encountered along the way (cache url, error).
    errors: Vec<(String, String)>,
}

/// Aggregated result of the substituter coverage pass.
struct CoverageOutcome {
    /// Paths some target substituter covers.
    coverage: BTreeSet<String>,
    /// Full store path → (cache url, narinfo) for relay-able paths.
    relay: HashMap<String, (String, NarInfo)>,
    /// Probe error counts per cache url.
    errors: BTreeMap<String, u64>,
}

/// Probe one path: target substituters first (any hit means the target can
/// fetch it itself), then — only when the path is not embedded in the
/// archive — the recording substituters in manifest order for a relay
/// source. Probe errors are reported but treated as "no answer" from that
/// cache (conservative: we may upload more than strictly needed).
async fn probe_path_coverage(
    path: String,
    embedded: bool,
    target_substituters: &[Substituter],
    src_substituters: &[Substituter],
) -> CoverageProbe {
    let hash = hash_part(&path).to_string();
    let mut errors: Vec<(String, String)> = Vec::new();

    for substituter in target_substituters {
        match substituter.narinfo(&hash).await {
            Ok(Some(_)) => {
                return CoverageProbe {
                    path,
                    target_hit: true,
                    relay: None,
                    errors,
                };
            }
            Ok(None) => {}
            Err(err) => errors.push((substituter.url(), format!("{err:#}"))),
        }
    }

    // Embedded paths are uploaded straight from the archive; no relay needed.
    if !embedded {
        for substituter in src_substituters {
            match substituter.narinfo(&hash).await {
                Ok(Some(narinfo)) => {
                    return CoverageProbe {
                        path,
                        target_hit: false,
                        relay: Some((substituter.url(), narinfo)),
                        errors,
                    };
                }
                Ok(None) => {}
                Err(err) => errors.push((substituter.url(), format!("{err:#}"))),
            }
        }
    }

    CoverageProbe {
        path,
        target_hit: false,
        relay: None,
        errors,
    }
}

/// Run the coverage probes for all `paths` with bounded concurrency and
/// aggregate the outcome. Each failing cache is warned about once, not once
/// per path.
async fn probe_coverage(
    paths: Vec<(String, bool)>,
    target_substituters: &[Substituter],
    src_substituters: &[Substituter],
    coverage_concurrency: usize,
) -> CoverageOutcome {
    let mut outcome = CoverageOutcome {
        coverage: BTreeSet::new(),
        relay: HashMap::new(),
        errors: BTreeMap::new(),
    };
    let mut warned: BTreeSet<String> = BTreeSet::new();
    let total = paths.len();
    let mut done = 0usize;

    let mut probes = futures_util::stream::iter(paths.into_iter().map(|(path, embedded)| {
        probe_path_coverage(path, embedded, target_substituters, src_substituters)
    }))
    .buffer_unordered(coverage_concurrency.max(1));

    while let Some(probe) = probes.next().await {
        done += 1;
        for (cache, error) in probe.errors {
            *outcome.errors.entry(cache.clone()).or_insert(0) += 1;
            if warned.insert(cache.clone()) {
                tracing::warn!(
                    cache = %cache,
                    error = %error,
                    "substituter narinfo probe failed; treating this cache as having no answer \
                     (paths may be uploaded unnecessarily)"
                );
            }
        }
        if probe.target_hit {
            outcome.coverage.insert(probe.path);
        } else if let Some((cache, narinfo)) = probe.relay {
            outcome.relay.insert(probe.path, (cache, narinfo));
        }
        // This is the longest context-building phase on big archives; show a
        // heartbeat so it doesn't read as a hang.
        if done.is_multiple_of(COVERAGE_PROGRESS_EVERY) {
            tracing::info!(
                done,
                total,
                covered = outcome.coverage.len(),
                relayable = outcome.relay.len(),
                "substituter coverage probe progress"
            );
        }
    }
    outcome
}

/// Probe which of `all_paths` the target already has, in
/// [`PrewarmConfig::probe_chunk`]-sized `QueryValidPaths` chunks spread
/// round-robin over up to [`PrewarmConfig::probe_concurrency`] daemon
/// channels held for the whole sub-phase. Failed chunks degrade to "not
/// present"; failing to open even one channel is a systemic error.
async fn probe_target_validity(
    pool: &GatewayPool,
    all_paths: &BTreeSet<String>,
    cfg: &PrewarmConfig,
) -> Result<BTreeSet<String>> {
    let chunk_size = cfg.probe_chunk.max(1);
    let mut chunks: Vec<Vec<String>> = Vec::new();
    let mut current: Vec<String> = Vec::with_capacity(chunk_size.min(all_paths.len()));
    for path in all_paths {
        current.push(path.clone());
        if current.len() == chunk_size {
            chunks.push(std::mem::take(&mut current));
        }
    }
    if !current.is_empty() {
        chunks.push(current);
    }
    if chunks.is_empty() {
        return Ok(BTreeSet::new());
    }

    // Open the probe channels once and hold them for the whole sub-phase.
    let wanted = cfg.probe_concurrency.clamp(1, chunks.len());
    let mut channels: Vec<DaemonChannel> = Vec::with_capacity(wanted);
    let mut last_error: Option<anyhow::Error> = None;
    for _ in 0..wanted {
        match pool.open_channel().await {
            Ok(channel) => channels.push(channel),
            Err(err) => last_error = Some(err),
        }
    }
    if channels.is_empty() {
        let error =
            last_error.unwrap_or_else(|| anyhow::anyhow!("no daemon channel could be opened"));
        return Err(error.context(
            "prewarm could not open any daemon channel for the target validity probe; \
             is the gateway reachable?",
        ));
    }
    if channels.len() < wanted {
        tracing::warn!(
            opened = channels.len(),
            wanted,
            "fewer probe channels than requested; probing with reduced concurrency"
        );
    }

    let worker_count = channels.len();
    let workers = channels.into_iter().enumerate().map(|(worker, channel)| {
        let worker_chunks: Vec<&[String]> = chunks
            .iter()
            .skip(worker)
            .step_by(worker_count)
            .map(Vec::as_slice)
            .collect();
        probe_worker(pool, channel, worker_chunks, cfg.op_timeout)
    });
    let outcomes = futures_util::future::join_all(workers).await;

    let mut valid = BTreeSet::new();
    let mut failed_chunks = 0u64;
    for (worker_valid, worker_failed) in outcomes {
        valid.extend(worker_valid);
        failed_chunks += worker_failed;
    }
    if failed_chunks > 0 {
        tracing::warn!(
            failed_chunks,
            "some validity probe chunks failed; their paths are treated as missing \
             (prewarm may upload more than necessary)"
        );
    }
    Ok(valid)
}

/// One validity-probe worker: run its chunks sequentially on its channel,
/// replacing the channel after an error (the wire position is unknown then).
/// Returns the valid paths it found and how many chunks failed.
async fn probe_worker(
    pool: &GatewayPool,
    channel: DaemonChannel,
    chunks: Vec<&[String]>,
    timeout: Duration,
) -> (BTreeSet<String>, u64) {
    let mut valid = BTreeSet::new();
    let mut failed_chunks = 0u64;
    let mut channel = Some(channel);
    for chunk in chunks {
        if channel.is_none() {
            match pool.open_channel().await {
                Ok(fresh) => channel = Some(fresh),
                Err(err) => {
                    failed_chunks += 1;
                    tracing::warn!(
                        error = %format!("{err:#}"),
                        paths = chunk.len(),
                        "could not reopen a validity probe channel; treating the chunk's paths \
                         as not present"
                    );
                    continue;
                }
            }
        }
        let open_channel = channel.as_mut().expect("channel was just ensured above");
        match open_channel.query_valid_paths(chunk, timeout).await {
            Ok(found) => valid.extend(found),
            Err(err) => {
                failed_chunks += 1;
                tracing::warn!(
                    error = %err,
                    connection = open_channel.connection_index(),
                    paths = chunk.len(),
                    "target validity probe chunk failed; treating its paths as not present"
                );
                // After any error the wire position is unknown; start fresh.
                channel = None;
            }
        }
    }
    (valid, failed_chunks)
}

/// Result of one upload sub-phase (large items, or one worker's share of a
/// level), merged into the context and report by [`apply_outcome`].
#[derive(Default)]
struct UploadOutcome {
    /// Successfully uploaded `(path, nar_size)` pairs.
    uploaded: Vec<(String, u64)>,
    /// `(path, error)` pairs for failed uploads / archive materialization.
    upload_failures: Vec<(String, String)>,
    /// `(path, error)` pairs for failed relay fetches.
    relay_failures: Vec<(String, String)>,
}

/// Fold an [`UploadOutcome`] into the shared context, the report, and the
/// set of planned paths whose upload failed (used to pre-skip dependents in
/// later levels).
fn apply_outcome(
    outcome: UploadOutcome,
    ctx: &mut SupplyContext,
    report: &mut PrewarmReport,
    failed_paths: &mut BTreeSet<String>,
) {
    for (path, nar_size) in outcome.uploaded {
        report.uploaded_paths += 1;
        report.uploaded_bytes += nar_size;
        ctx.target_valid.insert(path);
    }
    failed_paths.extend(
        outcome
            .upload_failures
            .iter()
            .chain(outcome.relay_failures.iter())
            .map(|(path, _)| path.clone()),
    );
    report.upload_failures.extend(outcome.upload_failures);
    report.relay_failures.extend(outcome.relay_failures);
}

/// Run-wide dial circuit breaker for the upload phases.
///
/// Channel-open failures are expected occasionally (the gateway drops idle
/// connections), but when every dial attempt fails consecutively the gateway
/// — or its port-forward — is gone, and without a breaker each remaining
/// sub-batch would burn another connect timeout on a doomed re-dial. Any
/// successful open resets the count; once tripped it stays tripped for the
/// rest of the run and remaining work is marked failed without further dial
/// attempts.
struct GatewayBreaker {
    /// Consecutive failed channel opens since the last success.
    consecutive_failures: AtomicUsize,
    /// Failure count at which the breaker trips.
    threshold: usize,
    /// Latched once the threshold is reached.
    tripped: AtomicBool,
}

impl GatewayBreaker {
    /// Breaker that trips after `threshold` consecutive failed channel opens.
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

    /// Record a successful channel open: the gateway is reachable, so the
    /// consecutive-failure count starts over.
    fn record_success(&self) {
        self.consecutive_failures.store(0, Ordering::Relaxed);
    }

    /// Record a failed channel open; trips (and warns, exactly once) when the
    /// threshold is reached.
    fn record_failure(&self) {
        let failures = self.consecutive_failures.fetch_add(1, Ordering::Relaxed) + 1;
        if failures >= self.threshold && !self.tripped.swap(true, Ordering::Relaxed) {
            tracing::warn!(
                consecutive_failures = failures,
                "prewarm dial circuit breaker tripped: the gateway looks unreachable; remaining \
                 uploads are marked failed without further dial attempts"
            );
        }
    }
}

/// Why a payload could not be materialized — decides which failure list of
/// the report the path lands in.
enum MaterializeError {
    /// The relay fetch from a recording substituter failed.
    Relay(String),
    /// The archive could not produce the NAR (dump failure or size mismatch).
    Archive(String),
}

/// Produce the NAR bytes for one batch item just before sending it.
async fn materialize_payload(
    archive: &Arc<ReplayArchive>,
    item: &UploadItem,
    substituters_by_url: &HashMap<String, &Substituter>,
) -> std::result::Result<Vec<u8>, MaterializeError> {
    match &item.payload {
        UploadPayload::DrvText(nar) => Ok(nar.clone()),
        UploadPayload::ArchivePath => {
            let archive = Arc::clone(archive);
            let store_path = item.store_path.clone();
            match tokio::task::spawn_blocking(move || archive.dump_nar(&store_path)).await {
                Ok(Ok(nar)) => Ok(nar),
                Ok(Err(err)) => Err(MaterializeError::Archive(format!(
                    "failed to NAR-serialize the embedded path: {err:#}"
                ))),
                Err(err) => Err(MaterializeError::Archive(format!(
                    "the archive NAR dump task panicked or was cancelled: {err}"
                ))),
            }
        }
        UploadPayload::Relay {
            substituter_url,
            narinfo,
        } => {
            let Some(substituter) = substituters_by_url.get(substituter_url) else {
                return Err(MaterializeError::Relay(format!(
                    "no configured recording substituter matches {substituter_url}"
                )));
            };
            substituter
                .fetch_nar(narinfo)
                .await
                .map_err(|err| MaterializeError::Relay(format!("{err:#}")))
        }
    }
}

/// Stream the large relayed paths to the target one by one (their NARs are
/// too big to buffer alongside a batch). Failures are recorded per path; the
/// daemon channel is replaced after any upload error.
async fn upload_large(
    pool: &GatewayPool,
    large: &[UploadItem],
    substituters_by_url: &HashMap<String, &Substituter>,
    breaker: &GatewayBreaker,
) -> UploadOutcome {
    let mut outcome = UploadOutcome::default();
    let mut channel: Option<DaemonChannel> = None;
    for item in large {
        // Once the gateway is declared unreachable there is no point fetching
        // multi-GB NARs that cannot be delivered.
        if channel.is_none() && breaker.is_tripped() {
            outcome
                .upload_failures
                .push((item.store_path.clone(), GATEWAY_UNREACHABLE.to_string()));
            continue;
        }
        let UploadPayload::Relay {
            substituter_url,
            narinfo,
        } = &item.payload
        else {
            // plan_uploads only routes relayed paths here; anything else is a
            // planner bug — degrade rather than abort.
            outcome.upload_failures.push((
                item.store_path.clone(),
                "internal: large upload item does not carry a relay payload".to_string(),
            ));
            continue;
        };
        let Some(substituter) = substituters_by_url.get(substituter_url) else {
            outcome.relay_failures.push((
                item.store_path.clone(),
                format!("no configured recording substituter matches {substituter_url}"),
            ));
            continue;
        };
        let (len, reader) = match substituter.fetch_nar_streaming(narinfo).await {
            Ok(stream) => stream,
            Err(err) => {
                outcome
                    .relay_failures
                    .push((item.store_path.clone(), format!("{err:#}")));
                continue;
            }
        };
        if channel.is_none() {
            if breaker.is_tripped() {
                outcome
                    .upload_failures
                    .push((item.store_path.clone(), GATEWAY_UNREACHABLE.to_string()));
                continue;
            }
            match pool.open_channel().await {
                Ok(fresh) => {
                    breaker.record_success();
                    channel = Some(fresh);
                }
                Err(err) => {
                    breaker.record_failure();
                    outcome.upload_failures.push((
                        item.store_path.clone(),
                        format!("no daemon channel for the streaming upload: {err:#}"),
                    ));
                    continue;
                }
            }
        }
        let open_channel = channel.as_mut().expect("channel was just ensured above");
        let entry = StoreEntry {
            store_path: item.store_path.clone(),
            info: item.info.clone(),
            nar: NarPayload::Reader { len, reader },
        };
        match open_channel
            .add_to_store_nar(entry, LARGE_UPLOAD_TIMEOUT)
            .await
        {
            Ok(()) => {
                tracing::info!(
                    path = %item.store_path,
                    bytes = item.info.nar_size,
                    "streamed large path to the target"
                );
                outcome
                    .uploaded
                    .push((item.store_path.clone(), item.info.nar_size));
            }
            Err(err) => {
                tracing::warn!(
                    path = %item.store_path,
                    error = %err,
                    "prewarm large-path upload failed; it falls back to per-request supply"
                );
                outcome.upload_failures.push((
                    item.store_path.clone(),
                    format!("streaming upload failed: {err}"),
                ));
                // The wire position is unknown after a failed upload; the
                // next item starts on a fresh channel.
                channel = None;
            }
        }
    }
    outcome
}

/// Shared, read-only inputs for one level's upload workers.
struct BatchUploadContext<'a> {
    /// The opened replay archive (payload source for embedded paths).
    archive: &'a Arc<ReplayArchive>,
    /// Pool to open daemon channels from.
    pool: &'a GatewayPool,
    /// Recording substituters by canonical URL (relay payload source).
    substituters_by_url: &'a HashMap<String, &'a Substituter>,
    /// Planned paths whose upload already failed — their dependents are
    /// skipped instead of being refused by the daemon.
    failed_paths: &'a BTreeSet<String>,
    /// Run-wide dial circuit breaker.
    breaker: &'a GatewayBreaker,
    /// Deadline for payload materialization and batch uploads.
    op_timeout: Duration,
    /// Completed sub-batch counter shared by the level's workers.
    progress: &'a AtomicUsize,
    /// Total sub-batches in the level (for progress lines).
    total_sub_batches: usize,
}

/// Upload one topological level: split it into sub-batches, spread the
/// sub-batches round-robin over the upload workers, and merge their results.
async fn upload_level(
    archive: &Arc<ReplayArchive>,
    pool: &GatewayPool,
    level_items: &[&UploadItem],
    substituters_by_url: &HashMap<String, &Substituter>,
    failed_paths: &BTreeSet<String>,
    breaker: &GatewayBreaker,
    cfg: &PrewarmConfig,
) -> UploadOutcome {
    let sub_batches = split_batches(level_items, cfg.batch_max_bytes, cfg.batch_max_entries);
    if sub_batches.is_empty() {
        return UploadOutcome::default();
    }
    // More workers than sub-batches (or than pool channels) would only sit
    // idle waiting for work or for a channel slot.
    let worker_count = cfg
        .upload_workers
        .clamp(1, pool.capacity())
        .min(sub_batches.len());

    let mut per_worker: Vec<Vec<Vec<&UploadItem>>> = vec![Vec::new(); worker_count];
    for (index, sub_batch) in sub_batches.iter().enumerate() {
        per_worker[index % worker_count].push(
            sub_batch
                .iter()
                .map(|&item_index| level_items[item_index])
                .collect(),
        );
    }

    let progress = AtomicUsize::new(0);
    let context = BatchUploadContext {
        archive,
        pool,
        substituters_by_url,
        failed_paths,
        breaker,
        op_timeout: cfg.op_timeout,
        progress: &progress,
        total_sub_batches: sub_batches.len(),
    };
    let workers = per_worker
        .into_iter()
        .map(|worker_batches| upload_worker(&context, worker_batches));
    let outcomes = futures_util::future::join_all(workers).await;

    let mut merged = UploadOutcome::default();
    for outcome in outcomes {
        merged.uploaded.extend(outcome.uploaded);
        merged.upload_failures.extend(outcome.upload_failures);
        merged.relay_failures.extend(outcome.relay_failures);
    }
    merged
}

/// One batch-upload worker: hold a single daemon channel, materialize each
/// assigned sub-batch just before sending it, and upload it with
/// `AddMultipleToStore`. A failed item drops out of its sub-batch; a failed
/// sub-batch is recorded and the worker continues on a fresh channel.
async fn upload_worker(
    context: &BatchUploadContext<'_>,
    sub_batches: Vec<Vec<&UploadItem>>,
) -> UploadOutcome {
    let mut outcome = UploadOutcome::default();
    let mut channel: Option<DaemonChannel> = None;
    for sub_batch in sub_batches {
        let done = context.progress.fetch_add(1, Ordering::Relaxed) + 1;
        if done.is_multiple_of(PROGRESS_EVERY) {
            tracing::info!(
                sub_batches = %format!("{done}/{}", context.total_sub_batches),
                "prewarm batch upload progress"
            );
        }

        // The gateway has been declared unreachable and this worker holds no
        // channel: don't waste relay fetches or dial attempts on the rest.
        if channel.is_none() && context.breaker.is_tripped() {
            outcome.upload_failures.extend(
                sub_batch
                    .iter()
                    .map(|item| (item.store_path.clone(), GATEWAY_UNREACHABLE.to_string())),
            );
            continue;
        }

        // Materialize payloads; a failed item drops out individually.
        let mut entries: Vec<StoreEntry> = Vec::with_capacity(sub_batch.len());
        let mut entry_meta: Vec<(String, u64)> = Vec::with_capacity(sub_batch.len());
        for item in &sub_batch {
            // An item whose reference already failed to upload would only be
            // refused by the daemon; skip it up front naming the real
            // culprit. Levels run in order, so this also covers transitive
            // dependents in later levels.
            let failed_reference = item.info.references.iter().find(|reference| {
                reference.as_str() != item.store_path.as_str()
                    && context.failed_paths.contains(reference.as_str())
            });
            if let Some(reference) = failed_reference {
                outcome.upload_failures.push((
                    item.store_path.clone(),
                    format!("reference {reference} failed its earlier upload — skipped"),
                ));
                continue;
            }
            // The materialization deadline keeps a stalled cache connection
            // from wedging the whole level (batch NARs are < 64 MiB by
            // construction, so op_timeout is a fair ceiling).
            let materialized = match tokio::time::timeout(
                context.op_timeout,
                materialize_payload(context.archive, item, context.substituters_by_url),
            )
            .await
            {
                Ok(result) => result,
                Err(_) => Err(MaterializeError::Relay(format!(
                    "fetch timed out after {}s",
                    context.op_timeout.as_secs()
                ))),
            };
            match materialized {
                Ok(nar) => {
                    if nar.len() as u64 != item.info.nar_size {
                        outcome.upload_failures.push((
                            item.store_path.clone(),
                            format!(
                                "materialized NAR is {} bytes but the path-info declares {}; \
                                 not uploaded",
                                nar.len(),
                                item.info.nar_size
                            ),
                        ));
                        continue;
                    }
                    entry_meta.push((item.store_path.clone(), item.info.nar_size));
                    entries.push(StoreEntry {
                        store_path: item.store_path.clone(),
                        info: item.info.clone(),
                        nar: NarPayload::Bytes(nar),
                    });
                }
                Err(MaterializeError::Relay(error)) => {
                    outcome
                        .relay_failures
                        .push((item.store_path.clone(), error));
                }
                Err(MaterializeError::Archive(error)) => {
                    outcome
                        .upload_failures
                        .push((item.store_path.clone(), error));
                }
            }
        }
        if entries.is_empty() {
            continue;
        }

        if channel.is_none() {
            if context.breaker.is_tripped() {
                outcome.upload_failures.extend(
                    entry_meta
                        .iter()
                        .map(|(path, _)| (path.clone(), GATEWAY_UNREACHABLE.to_string())),
                );
                continue;
            }
            match context.pool.open_channel().await {
                Ok(fresh) => {
                    context.breaker.record_success();
                    channel = Some(fresh);
                }
                Err(err) => {
                    context.breaker.record_failure();
                    // A channel-open failure means the gateway (or this
                    // connection) is unavailable right now — record the
                    // sub-batch and keep going; the next sub-batch retries
                    // (until the breaker trips).
                    let reason = format!("no daemon channel for the batch upload: {err:#}");
                    outcome.upload_failures.extend(
                        entry_meta
                            .iter()
                            .map(|(path, _)| (path.clone(), reason.clone())),
                    );
                    continue;
                }
            }
        }
        let open_channel = channel.as_mut().expect("channel was just ensured above");
        match open_channel
            .add_multiple_to_store(entries, context.op_timeout)
            .await
        {
            Ok(()) => outcome.uploaded.extend(entry_meta),
            Err(err) => {
                let reason = format!("batch upload failed: {err}");
                tracing::warn!(
                    error = %err,
                    connection = open_channel.connection_index(),
                    paths = entry_meta.len(),
                    "prewarm batch upload failed; its paths fall back to per-request supply"
                );
                outcome.upload_failures.extend(
                    entry_meta
                        .into_iter()
                        .map(|(path, _)| (path, reason.clone())),
                );
                // Refusals and transport errors both leave the wire position
                // unknown; the next sub-batch starts on a fresh channel.
                channel = None;
            }
        }
    }
    outcome
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};

    use rio_nix::protocol::pathinfo::ValidPathInfo;

    use super::*;

    const DEP_DRV: &str = "/nix/store/a1111111111111111111111111111111-dep.drv";
    const APP_DRV: &str = "/nix/store/a2222222222222222222222222222222-app.drv";
    const IMPURE_DRV: &str = "/nix/store/a3333333333333333333333333333333-impure.drv";
    const CACHED_DRV: &str = "/nix/store/a4444444444444444444444444444444-cached.drv";
    const SRC: &str = "/nix/store/b1111111111111111111111111111111-src.txt";
    const DEP_OUT: &str = "/nix/store/c1111111111111111111111111111111-dep";
    const APP_OUT: &str = "/nix/store/c2222222222222222222222222222222-app";

    fn fixture() -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/replay/basic")
    }

    /// Hand-made batch item: only the store path, NAR size, and references
    /// matter for the pure helpers under test.
    fn item(store_path: &str, nar_size: u64, references: &[&str]) -> UploadItem {
        UploadItem {
            store_path: store_path.to_string(),
            info: ValidPathInfo {
                deriver: None,
                nar_hash: vec![0u8; 32],
                references: references.iter().map(|r| r.to_string()).collect(),
                registration_time: 0,
                nar_size,
                ultimate: false,
                signatures: Vec::new(),
                content_address: None,
            },
            payload: UploadPayload::DrvText(Vec::new()),
        }
    }

    #[test]
    fn topo_levels_respects_references() {
        let src = "/nix/store/t1111111111111111111111111111111-src";
        let drv = "/nix/store/t2222222222222222222222222222222-thing.drv";
        let out_first = "/nix/store/t3333333333333333333333333333333-out1";
        let out_second = "/nix/store/t4444444444444444444444444444444-out2";
        let already_valid = "/nix/store/t5555555555555555555555555555555-on-target";

        // The drv's only reference is already valid on the target, so it has
        // no planned references and stays independent (level 0); the second
        // output references its sibling output, pushing it one level deeper.
        let batch = vec![
            item(src, 10, &[]),
            item(drv, 10, &[already_valid]),
            item(out_first, 10, &[src]),
            item(out_second, 10, &[src, out_first]),
        ];
        let target_valid: BTreeSet<String> = [already_valid.to_string()].into_iter().collect();

        let levels = topo_levels(&batch, &target_valid);

        // src + the independent drv at level 0 (batch order preserved), the
        // first output at level 1, the sibling-referencing output at level 2.
        assert_eq!(levels, vec![vec![0, 1], vec![2], vec![3]]);
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
        // upload phases never dial again afterwards).
        breaker.record_failure();
        assert!(breaker.is_tripped());
        breaker.record_success();
        assert!(breaker.is_tripped());
    }

    #[test]
    fn split_batches_caps_bytes_and_entries() {
        let a = item("/nix/store/s1111111111111111111111111111111-a", 100, &[]);
        let b = item("/nix/store/s2222222222222222222222222222222-b", 100, &[]);
        let c = item("/nix/store/s3333333333333333333333333333333-c", 300, &[]);
        let d = item("/nix/store/s4444444444444444444444444444444-d", 50, &[]);
        let items: Vec<&UploadItem> = vec![&a, &b, &c, &d];
        // The byte cap closes [a, b]; c exceeds the cap on its own and gets
        // its own sub-batch; d starts a fresh one.
        assert_eq!(
            split_batches(&items, 250, 3),
            vec![vec![0, 1], vec![2], vec![3]]
        );

        // A single oversized item is its own sub-batch.
        let big = item("/nix/store/s5555555555555555555555555555555-big", 1000, &[]);
        let items: Vec<&UploadItem> = vec![&big];
        assert_eq!(split_batches(&items, 250, 3), vec![vec![0]]);

        // The entry cap also closes sub-batches when bytes never would.
        let small: Vec<UploadItem> = (0..5)
            .map(|index| {
                item(
                    &format!("/nix/store/s666666666666666666666666666666{index}-small{index}"),
                    1,
                    &[],
                )
            })
            .collect();
        let items: Vec<&UploadItem> = small.iter().collect();
        assert_eq!(
            split_batches(&items, 1024 * 1024, 2),
            vec![vec![0, 1], vec![2, 3], vec![4]]
        );
    }

    /// Offline supply context on the fixture archive: no substituters, no
    /// network, no target — exactly what `--dry-run` does. Also exercises
    /// the spawn_blocking closure-walk path.
    #[tokio::test]
    async fn build_supply_context_offline_smoke() {
        let archive = Arc::new(ReplayArchive::open(&fixture()).unwrap());

        // Roots exactly as the replay engine derives them: deduped drv paths
        // in recorded-offset order.
        let mut roots: Vec<String> = Vec::new();
        for request in archive.requests() {
            for (drv_path, _outputs) in &request.paths {
                if !roots.contains(drv_path) {
                    roots.push(drv_path.clone());
                }
            }
        }

        let (ctx, closure) = build_supply_context(&archive, &roots, &[], &[], 8)
            .await
            .unwrap();

        // Workload: dep + app (impure is demoted via impure-env.json, cached
        // never had a build record), and exactly their outputs are withheld.
        let expected_workload: BTreeSet<String> = [DEP_DRV.to_string(), APP_DRV.to_string()]
            .into_iter()
            .collect();
        assert_eq!(ctx.workload.drvs, expected_workload);
        let expected_outputs: BTreeSet<String> = [DEP_OUT.to_string(), APP_OUT.to_string()]
            .into_iter()
            .collect();
        assert_eq!(ctx.workload_outputs, expected_outputs);

        // No substituters were given, so nothing was probed.
        assert!(ctx.target_coverage.is_empty());
        assert!(ctx.relay_narinfos.is_empty());
        assert!(ctx.target_valid.is_empty());
        assert!(ctx.probe_errors.is_empty());

        // The union closure covers all four fixture derivations.
        for drv_path in [DEP_DRV, APP_DRV, IMPURE_DRV, CACHED_DRV] {
            assert!(
                closure.topo.iter().any(|node| node.drv_path == drv_path),
                "closure must contain {drv_path}"
            );
            assert!(closure.all_paths.contains(drv_path));
        }
        assert_eq!(closure.topo.len(), 4);
        assert!(closure.all_paths.contains(SRC));
    }
}
