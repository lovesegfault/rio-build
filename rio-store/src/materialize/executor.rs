//! Per-job materialization execution: tenant re-resolution → live
//! wanted-set read → in-process reference-closure walk → pin-at-ingest
//! → outcome (substitution-replacement design §2.2 items 2–4, §5.1).
//!
//! The control loop the scheduler's `walk_substitute_closure` used to
//! be — now in-process next to the machinery it drives
//! ([`Substituter::try_substitute`], [`Substituter::check_available`],
//! the admission gate). The store substitutes ONE path per
//! `try_substitute` call (no closure walk — that is its documented
//! contract), so this module owns closure completeness.
// r[impl store.materialize.executor+5]

use std::collections::{HashMap, HashSet, VecDeque};
use std::time::Duration;

use sqlx::PgPool;
use tracing::{debug, info, warn};
use uuid::Uuid;

use rio_evidence_kernel::outcome::{
    LoopControl, SubstituteFailureClass, TenantAttemptCells, TenantAttemptsVerdict,
};
use rio_proto::types::{MaterializationOutcome, materialization_outcome};

use crate::substitute::Substituter;
use crate::visibility::{SharedTrustCache, TenantVisible, visible_to_tenant};

use super::client::ClaimedJob;

/// Closure-walk node cap — the same bound the scheduler's
/// `closure_cap_exceeded` (actor/dispatch.rs) enforces, from the same
/// shared constant. A hostile upstream's reference chain must not walk
/// unboundedly on the store side either.
const CLOSURE_WALK_CAP: usize = rio_common::limits::MAX_SUBSTITUTE_CLOSURE;

/// Per-path deadline for the miss-classification probe
/// ([`Substituter::check_available`] on a single path).
const MISS_PROBE_DEADLINE: Duration = Duration::from_secs(30);

/// What one job execution needs from the store process: the shared PG
/// pool (scheduler tables: jobs/wanted/builds/pins) and this replica's
/// substitution machinery.
pub struct ExecutorContext {
    pub pool: PgPool,
    pub substituter: std::sync::Arc<Substituter>,
    /// Per-job path-resolution window width F (live_047/R-C,
    /// `store.materialize.path-fold+1`): at most this many path futures
    /// in flight per walk. 1 = the serial walk as the F=1 instance of
    /// the one code shape (the driver/window structure is identical;
    /// only the width changes). Floored at 1 by the driver.
    pub path_fanout: usize,
    /// The pod-wide path-slot pool (`store.materialize.gate-share+1`):
    /// every path future holds one slot for its lifetime; main.rs
    /// sizes it at effective_admission_cap / 2 and threads ONE pool
    /// through every worker's context.
    pub path_slots: PathSlotPool,
    /// Test-only probe rendezvous: when set, every `probe_local` call
    /// awaits this barrier at entry. The width-4 probe-concurrency
    /// red sizes it at F and watches whether siblings can rendezvous
    /// (they cannot while any job-wide guard spans the probe).
    /// `None` everywhere outside the owning tests — semantics-neutral.
    #[cfg(test)]
    pub probe_rendezvous: Option<std::sync::Arc<tokio::sync::Barrier>>,
}

impl ExecutorContext {
    /// THE context constructor (every construction site routes here —
    /// the test-seam fields stay out of production literals).
    pub fn new(
        pool: PgPool,
        substituter: std::sync::Arc<Substituter>,
        path_fanout: usize,
        path_slots: PathSlotPool,
    ) -> Self {
        Self {
            pool,
            substituter,
            path_fanout,
            path_slots,
            #[cfg(test)]
            probe_rendezvous: None,
        }
    }
}

/// Execute one claimed materialization job end-to-end and produce the
/// outcome to report.
///
/// Order (design §2.2 items 2–4, all against LIVE state — never a
/// creation-time snapshot):
///
/// 1. **Tenant re-resolution** (AS-4 / adjudication PDQ-8): the
///    recorded creating-build tenant ([`ClaimedJob::tenant_hint`]) is
///    honored only while a live interested build still carries it;
///    otherwise any live interested build's tenant; otherwise
///    `InfraFailure{no-tenant-context}` — never a silent miss, never
///    `Unobtainable`.
/// 2. **Live wanted set**: `build_wanted_outputs` of live builds,
///    saturated by the `'{}'` = all-outputs convention, mapped to
///    store paths through the derivation row's parallel arrays.
/// 3. **Closure walk**: BFS over narinfo references from the wanted
///    seeds; every path `try_substitute`d in-process; every
///    ingested/verified path pinned (`pin_kind='materialization'`)
///    BEFORE the Success report — closure-complete or no Success.
/// 4. **Final verification pass**: the wanted set is RE-READ after the
///    walk; growth re-enters the walk (the loop), so the reported
///    coverage is against execution-end live wanted, not a snapshot.
// r[impl store.materialize.executor+5]
// r[impl sched.materialize.pinning]
pub async fn execute_job(
    ctx: &ExecutorContext,
    claimed: &ClaimedJob,
    admission: ClaimAdmission,
) -> CountedOutcome {
    execute_job_with_progress(ctx, claimed, admission, |_, _, _| {}).await
}

/// [`execute_job`] with a byte-progress callback (BC-4 / Phase B).
///
/// `on_progress(bytes_done, bytes_expected, upstream_uri)` streams
/// CUMULATIVE byte counts across the job's whole closure walk —
/// committed paths' NAR sizes plus the current path's streamed bytes
/// (merged_bug_195: multiple calls per path during the body fetch).
/// COMMITTED-FLOOR semantics, ENFORCED by `MonotoneProgress`, the
/// only constructor of emission sites
/// (`store.materialize.progress-monotone+1`): `done <= expected` at
/// every call; `done` never drops below the committed floor, which
/// only the per-path success witness raises; display MAY step back
/// from a FAILED attempt's provisional peak (truthful display — the
/// dead bytes were never committed; bug_159's within-path retry
/// regression stays impossible relative to committed work). The
/// final call covers the whole closure. Display-only and droppable:
/// the callback must be cheap and non-blocking (it runs on the
/// walk); the caller forwards it to
/// `ReportMaterializationProgress` fire-and-forget.
// r[impl store.materialize.executor+5]
// r[impl obs.metric.store]
pub async fn execute_job_with_progress(
    ctx: &ExecutorContext,
    claimed: &ClaimedJob,
    admission: ClaimAdmission,
    on_progress: impl Fn(u64, u64, &str) + Send + Sync + 'static,
) -> CountedOutcome {
    CountedOutcome::count(execute_job_inner(ctx, claimed, admission, on_progress).await)
}

/// Witness that a [`MaterializationOutcome`] passed the ONE counting
/// chokepoint (merged_bug_115). [`super::client::report_until_acked`]
/// DEMANDS it in its signature, so an uncounted report does not
/// typecheck: the SIGTERM arm used to synthesize `Aborted` straight
/// onto the wire, leaving `outcome="aborted"` seeded, HELP'd, and
/// documented as live while its only producer bypassed the counter
/// the bug_244 totality close blessed. A voluntary helper both paths
/// merely *may* call is the recurrence shape this type forbids.
pub struct CountedOutcome {
    outcome: MaterializationOutcome,
}

impl CountedOutcome {
    /// THE count-and-report mint — the only constructor. T-6.2
    /// lifecycle counter: one increment per finished (or synthesized)
    /// execution, labeled through the ONE alphabet mapping (bug_244).
    pub(crate) fn count(outcome: MaterializationOutcome) -> Self {
        metrics::counter!(
            "rio_store_materialization_executions_total",
            "outcome" => outcome_label(outcome.outcome.as_ref())
        )
        .increment(1);
        Self { outcome }
    }

    /// Consume the witness into the wire outcome (the report path).
    pub(crate) fn into_outcome(self) -> MaterializationOutcome {
        self.outcome
    }
}

/// The outcome-label alphabet of
/// `rio_store_materialization_executions_total` — THE single source
/// (bug_244). The boot-time seed loop iterates this const, the emit
/// chokepoint maps through [`outcome_label`] (whose exhaustive match
/// is the only variant→label mapping), and the HELP string
/// interpolates it (`executions_help` in the parent module); the drift test pins
/// const == match image, so a sixth outcome that misses any tier
/// fails to compile (non-exhaustive match) or fails the drift test
/// (label not seeded / not helped). Pre-fix the three tiers
/// enumerated the alphabet independently: `retry_later` was emitted
/// but never seeded (its series was born at first increment — every
/// rate()/increase() panel missed the first deferral burst after
/// every rollout) and the HELP still advertised the original three
/// labels.
pub(crate) const OUTCOME_LABELS: [&str; 5] =
    ["success", "unobtainable", "infra", "aborted", "retry_later"];

/// The ONLY `MaterializationOutcome` → metric-label mapping (bug_244).
/// Total over the oneof alphabet plus the absent case — `None` counts
/// as `infra` (a report with no outcome is an infrastructure shape,
/// matching the consumer's treatment).
pub(crate) fn outcome_label(outcome: Option<&materialization_outcome::Outcome>) -> &'static str {
    match outcome {
        Some(materialization_outcome::Outcome::Success(_)) => "success",
        Some(materialization_outcome::Outcome::Unobtainable(_)) => "unobtainable",
        Some(materialization_outcome::Outcome::InfraFailure(_)) | None => "infra",
        // Synthesized by the claim loop's SIGTERM arm, never by the
        // walk itself — and counted like every other outcome BECAUSE
        // the arm must mint a `CountedOutcome` to reach
        // `report_until_acked` (merged_bug_115: pre-fix the arm
        // bypassed the counter and this comment asserted the
        // opposite of the code).
        Some(materialization_outcome::Outcome::Aborted(_)) => "aborted",
        // Transient, uncharged retry (merged_bug_178): raced placeholder
        // or upstream 429 — counted so the dashboard sees deferral
        // rates next to the charged classes.
        Some(materialization_outcome::Outcome::RetryLater(_)) => "retry_later",
    }
}

/// The pure clamp law behind `MonotoneProgress` (bug_159 + bug_087):
/// given the job's COMMITTED floor and an absolute candidate report,
/// the emitted pair is `done = max(floor, done)`,
/// `expected = max(expected, done)` — emitted `done` never drops
/// below completed work and `done <= expected` holds at every call.
/// Provisional emissions may step back relative to a FAILED attempt's
/// earlier peak (truthful display; the dead bytes were never
/// committed); monotonicity is guaranteed relative to the committed
/// floor, which only [`MonotoneProgress::commit`] raises.
/// Proptest-swept below.
fn clamp_progress(high_water: u64, done: u64, expected: u64) -> (u64, u64) {
    let emit_done = high_water.max(done);
    let emit_expected = expected.max(emit_done);
    (emit_done, emit_expected)
}

/// bug_159 + bug_087: the job-level monotone progress adapter — the
/// raw job callback is moved in and private, so an unclamped emission
/// site is unwritable. Owns the job's COMMITTED floor and routes every
/// emission through [`clamp_progress`]. live_047/R-C (path-fold law
/// 6): the DRIVER is the sole caller — path futures emit only EVENTS
/// ([`ProgressTick`]) onto the driver's stream, so commits and
/// provisional emissions form one total order (the type and clamp law
/// are untouched; what moved is WHO calls it).
///
/// Two emission classes, one law (bug_087): the job-level floor is
/// mutated ONLY by [`Self::commit`] — the success witness, called
/// when a path is fully processed — while per-path streaming
/// emissions are PROVISIONAL: clamped against the committed floor
/// (never below it) but unable to raise it. Pre-fix, `emit`
/// fetch_max'ed every provisional candidate into the job-wide
/// high-water, so a large partial stream from an attempt that later
/// FAILED permanently floored the job: `clamp_progress` dragged
/// `expected` up to the inflated `done`, and the final BC-4 report
/// showed `done == expected` above the closure's true byte total.
/// Display may step back after a failed attempt (truthful); the
/// committed floor never regresses.
// r[impl store.materialize.progress-monotone+1]
struct MonotoneProgress<F: Fn(u64, u64, &str) + Send + Sync + 'static> {
    on_progress: std::sync::Arc<F>,
    /// Job-level COMMITTED floor: bytes of fully-processed paths.
    /// Atomic because the per-path callbacks are `Fn + Send + Sync`
    /// by the substituter's callback contract.
    committed_floor: std::sync::Arc<std::sync::atomic::AtomicU64>,
}

impl<F: Fn(u64, u64, &str) + Send + Sync + 'static> MonotoneProgress<F> {
    fn new(on_progress: F) -> Self {
        MonotoneProgress {
            on_progress: std::sync::Arc::new(on_progress),
            committed_floor: std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0)),
        }
    }

    /// The SUCCESS WITNESS (bug_087): fold a fully-processed prefix
    /// into the job floor and emit the completed tick. The only
    /// mutation site of job-level progress state.
    fn commit(&self, completed_total: u64, uri: &str) {
        let prev = self
            .committed_floor
            .fetch_max(completed_total, std::sync::atomic::Ordering::SeqCst);
        let (d, e) = clamp_progress(prev, completed_total, completed_total);
        (self.on_progress)(d, e, uri);
    }

    /// A PROVISIONAL emission: clamped to the committed floor
    /// (display never drops below completed work) but structurally
    /// unable to raise it — a failed attempt's streamed bytes leave
    /// no trace on job-level state.
    fn emit_provisional(&self, done: u64, expected: u64, uri: &str) {
        let floor = self
            .committed_floor
            .load(std::sync::atomic::Ordering::SeqCst);
        let (d, e) = clamp_progress(floor, done, expected);
        (self.on_progress)(d, e, uri);
    }
}

/// The ONE frontier admission gate (merged_bug_003): `visited` is
/// inserted at ENQUEUE — frontier membership witnesses spawnable work
/// (`!frontier.is_empty()` ⇒ the next pop spawns) — and the
/// closure-walk cap is enforced here, at the growth point, latching
/// the same driver-synthesized Charge evidence into the same
/// latch+fold as any abort. Duplicates (closure diamonds under pool
/// contention; cross-chain new_seeds × reseed arrivals) are dropped
/// at the door; the spawn-time insert this replaces degenerated to
/// the pop-side debug_assert.
fn enqueue_path(
    visited: &mut HashSet<String>,
    frontier: &mut VecDeque<(String, PathCell)>,
    walk_abort: &mut Option<Vec<AbortEvidence>>,
    path: String,
    cell: PathCell,
) {
    if !visited.insert(path.clone()) {
        return;
    }
    if visited.len() > CLOSURE_WALK_CAP {
        // Driver-synthesized charge evidence; enters the same latch +
        // tier fold as any abort. Checked at the enqueue (the cap
        // bounds enqueued-unique paths; trip timing is ≤ one
        // in-flight window earlier than the old spawn-time check).
        walk_abort.get_or_insert_with(Vec::new).push(AbortEvidence {
            path,
            grade: AbortDisposition::Charge {
                detail: format!(
                    "closure walk exceeded {CLOSURE_WALK_CAP} paths \
                     (hostile upstream reference chain?)"
                ),
            },
        });
        return;
    }
    frontier.push_back((path, cell));
}

/// The walk body behind [`execute_job_with_progress`] (split so the
/// outcome counter has a single increment site over every return path).
async fn execute_job_inner(
    ctx: &ExecutorContext,
    claimed: &ClaimedJob,
    admission: ClaimAdmission,
    on_progress: impl Fn(u64, u64, &str) + Send + Sync + 'static,
) -> MaterializationOutcome {
    use futures_util::StreamExt as _;
    // bug_159: every emission goes through the monotone adapter — the
    // raw callback is moved in, so an unclamped site is unwritable.
    // (`SubstProgressFn` is `dyn Fn + 'static`, so the per-path
    // closures own Arc handles, cloned per path.)
    let progress = MonotoneProgress::new(on_progress);

    // ── 1–4. Walk loop with final-verification re-read ───────────────
    // bug_115: the per-job trusted-set memo for the local-visibility
    // probe (two PG queries per tenant, amortized across the walk).
    // bug_073: shared PER-OPERATION — `SharedTrustCache` locks
    // internally around pure map ops (the guard never escapes and
    // never spans an await; a cross-await hold is a COMPILE error —
    // path futures are `Send` by the window's BoxFuture bound and the
    // internal guard is `!Send`), with per-tenant single-flight loads
    // so concurrent missing siblings coalesce on one load. The probe
    // and the substitute-hit re-check share this one handle under one
    // lock discipline; F sibling probes overlap freely.
    let trust_cache = SharedTrustCache::default();
    // bug_102 (slot ≺ claim): the claim carried its FIRST slot in —
    // iteration 1's width-0 spawn consumes it without touching the
    // baseline FIFO, so the interval between claim and first spawn
    // contains zero slot waits. A walk that never spawns (empty first
    // frontier / early returns) drops it — slot returned to the pool.
    let mut first_slot: Option<PathSlot> = Some(admission.first_slot);
    let mut visited: HashSet<String> = HashSet::new();
    let mut ingested: Vec<String> = Vec::new();
    let mut verified: Vec<String> = Vec::new();
    // bug_139 / signed Q2: the per-path verified-tenant sets — filled
    // at the local-present arm (the witness's full set) and the
    // substitute-hit arm (serving tenant + post-ingest re-check);
    // carried to the scheduler on the Success wire.
    let mut verified_tenants_by_path: HashMap<String, Vec<Uuid>> = HashMap::new();
    // merged_bug_193: wanted-miss vs closure-reference-miss are
    // DIFFERENT verdicts at the consumer (a covered root over a
    // punctured closure must never complete) — track the cell per
    // frontier entry.
    // bug_266: every per-path verdict is GENERATION-STAMPED with the
    // tenant-set generation it was reached under (kernel
    // GenStampedCells — record() is the only writer, so an unstamped
    // verdict is unrepresentable). When the live tenant set GROWS
    // mid-walk, stale cells drain back into the frontier (the new
    // tenant's upstreams get their owner-Q2 chance) and the outcome
    // compiler REFUSES to fold cells older than the final generation.
    let mut missing_wanted = rio_evidence_kernel::outcome::GenStampedCells::new();
    let mut missing_references = rio_evidence_kernel::outcome::GenStampedCells::new();
    // merged_bug_005: paths refused on TRUST (present upstream, no
    // verifiable signature under any interested tenant). They ride
    // the same missing cells (the settlement is still Unobtainable —
    // from-source), but the cause string must name the refusal so an
    // operator fixes trusted_keys instead of chasing a phantom miss.
    let mut trust_refused = rio_evidence_kernel::outcome::GenStampedCells::new();
    // merged_bug_046: paths whose settle cause is the AlreadyComplete
    // content disagreement (mirrors trust_refused one axis over).
    let mut content_mismatched = rio_evidence_kernel::outcome::GenStampedCells::new();
    // bug_266: the tenant-set generation. Bumps when the freshly
    // resolved set contains a member the previous resolve did not —
    // ONLY growth re-opens verdicts (shrinkage cannot turn a miss
    // into a hit; the departed tenant's evidence stands).
    let mut tenant_generation: u64 = 0;
    let mut last_tenants: std::collections::BTreeSet<Uuid> = std::collections::BTreeSet::new();
    // Reference-cell paths drained by a growth re-enter the frontier
    // directly (their parent edge was already walked; wanted-cell
    // paths re-enter via the wanted re-read once cleared from
    // `visited`).
    let mut reseed_references: Vec<String> = Vec::new();
    // BC-4 cumulative progress accounting: bytes of fully-processed
    // paths (the committed floor). merged_bug_012: the driver's
    // per-iteration WindowProgress fold adds Σ streamed / Σ declared
    // over the WHOLE in-flight window on top of this floor — so
    // reports are cumulative across the closure, non-decreasing in
    // done on success traces at any width, and expected genuinely
    // leads done while declared work is outstanding (merged_bug_195;
    // the narinfo/local-row read precedes the body fetch).
    let mut completed_bytes: u64 = 0;
    let mut first_iteration = true;

    loop {
        // bug_041: ONE freshness authority per iteration — the tenant
        // set and the wanted set are re-read TOGETHER from the same
        // live-interest relation (AS-4, live-interest-first; PLURAL
        // per merged_bug_028 / owner Q2: any interested tenant's
        // upstreams may serve, the job fails only when none can).
        // Pre-fix the tenants were snapshotted once before the loop,
        // so seeds arriving from a mid-walk interest shift were
        // probed under the DEPARTED tenant set and compiled to
        // Unobtainable without the live tenant's upstreams ever being
        // consulted — two views of one live relation at different
        // ages.
        let tenants = match resolve_tenants(ctx, claimed).await {
            Ok(t) if t.is_empty() => {
                // The AS-4 posture: no resolvable tenant context is an
                // infrastructure condition (upstream selection is
                // impossible), NEVER an Unobtainable verdict and never
                // a silent skip. Mid-walk this is interest vanishing
                // entirely — the scheduler re-arms.
                return infra_failure(format!(
                    "no-tenant-context: no live interested build carries a tenant \
                     for {} (recorded hint: {:?})",
                    claimed.drv_hash, claimed.tenant_hint
                ));
            }
            Ok(t) => t,
            Err(e) => {
                return infra_failure(format!("tenant resolution query failed: {e}"));
            }
        };
        // bug_266: tenant-set growth detection. A grown set re-opens
        // every verdict reached under an older generation: stale
        // cells drain (all four registries), the drained paths leave
        // `visited` (wanted ones re-seed via the wanted re-read;
        // reference ones re-enter the frontier directly below).
        {
            let current: std::collections::BTreeSet<Uuid> = tenants.iter().copied().collect();
            let grew = current.difference(&last_tenants).next().is_some();
            if grew && !last_tenants.is_empty() {
                tenant_generation += 1;
                let stale_wanted = missing_wanted.drain_stale(tenant_generation);
                let stale_refs = missing_references.drain_stale(tenant_generation);
                let _ = trust_refused.drain_stale(tenant_generation);
                let _ = content_mismatched.drain_stale(tenant_generation);
                if !stale_wanted.is_empty() || !stale_refs.is_empty() {
                    info!(
                        drv_hash = %claimed.drv_hash,
                        generation = tenant_generation,
                        reopened_wanted = stale_wanted.len(),
                        reopened_references = stale_refs.len(),
                        "tenant set grew mid-walk; re-probing settled verdicts \
                         under the live set (bug_266)"
                    );
                }
                for p in &stale_wanted {
                    visited.remove(p);
                }
                for p in stale_refs {
                    visited.remove(&p);
                    reseed_references.push(p);
                }
            } else if last_tenants.is_empty() {
                // First resolve: generation 0 is the baseline.
            }
            last_tenants = current;
        }
        // The live wanted read: first iteration = the at-claim read;
        // subsequent iterations = the final verification passes
        // (design §2.2 item 3: never snapshot at creation).
        let wanted = match live_wanted_paths(ctx, claimed).await {
            Ok(w) => w,
            Err(e) => return infra_failure(format!("wanted-set query failed: {e}")),
        };
        // merged_bug_194 (store leg): a FIRST iteration with no
        // verifiable wanted path can never produce a meaningful
        // Success — "Success with nothing verified" is exactly the
        // vacuous completion the LiveWanted witness forbids scheduler-
        // side. Report infra (the scheduler re-arms; interest may
        // still be materializing) instead of walking nothing.
        if first_iteration && wanted.is_empty() {
            return infra_failure(format!(
                "no-verifiable-wanted-paths: live interest for {} resolves to no \
                 verifiable store path (floating-CA placeholders or no live build)",
                claimed.drv_hash
            ));
        }
        first_iteration = false;
        let new_seeds: Vec<String> = wanted
            .into_iter()
            .filter(|p| !visited.contains(p))
            .collect();
        if new_seeds.is_empty() && reseed_references.is_empty() {
            // The final verification pass found no growth: coverage is
            // complete against execution-end live wanted.
            break;
        }

        // The abort latch (law 4): Some = stop spawning; the Vec is
        // the completed abort-grade evidence the tier fold consumes.
        // Declared BEFORE the frontier build: the closure-walk cap is
        // enforced at the ENQUEUE sites (merged_bug_003), and the
        // build is the first of them.
        let mut walk_abort: Option<Vec<AbortEvidence>> = None;

        // Frontier entries carry their CELL: live-wanted seeds vs
        // narinfo reference extensions (merged_bug_193). bug_266:
        // drained reference verdicts re-enter with their original
        // cell — the consumer's covered-root-over-punctured-closure
        // law keeps holding. merged_bug_003: every entry passes
        // [`enqueue_path`] — `visited` is inserted at ENQUEUE, so a
        // path arriving via BOTH chains in one iteration (wanted-set
        // growth racing a bug_266 drain) is admitted exactly once.
        let mut frontier: VecDeque<(String, PathCell)> = VecDeque::new();
        for (p, cell) in new_seeds.into_iter().map(|p| (p, PathCell::Wanted)).chain(
            reseed_references
                .drain(..)
                .map(|p| (p, PathCell::Reference)),
        ) {
            enqueue_path(&mut visited, &mut frontier, &mut walk_abort, p, cell);
        }
        // ── Path-axis evidence law (store.materialize.path-fold+1) ──
        // ONE driver (this function) owns ALL job state; per-path
        // resolution runs as evidence-returning futures in an
        // F-bounded window, spawned in frontier order, applied in
        // COMPLETION order. The window, the tick stream, and their
        // borrows of this iteration's tenant set are PER-ITERATION
        // locals, so the generation barrier (law 5) holds by
        // construction — no path future is in flight across the
        // tenant re-resolve; every cell recorded in iteration k
        // carries generation g_k (bug_266's fold_guard holds with
        // zero changes; barrier-by-construction is the structural
        // form of the re-resolve assert).
        let fanout = ctx.path_fanout.max(1);
        let (tick_tx, mut tick_rx) =
            tokio::sync::mpsc::channel::<ProgressTick>(PROGRESS_TICK_QUEUE);
        // merged_bug_012: the per-iteration window fold — the driver
        // owns the cumulative arithmetic; futures emit raw counters.
        let mut window_progress = WindowProgress::default();
        let mut window: futures_util::stream::FuturesUnordered<
            futures_util::future::BoxFuture<'_, PathResolution>,
        > = futures_util::stream::FuturesUnordered::new();

        // The apply chokepoint: a TOTAL match over PathResolution
        // (the variant-totality census — zero `_` arms), driver-side
        // only; mutates exclusively driver-owned state.
        macro_rules! apply_resolution {
            ($res:expr) => {
                match $res {
                    PathResolution::Served {
                        path,
                        nar_size,
                        references,
                        verified_tenants: vt,
                        ingested: was_ingested,
                    } => {
                        // merged_bug_012: leave the fold BEFORE the
                        // commit below — late ticks are zombies.
                        window_progress.retire(&path);
                        verified_tenants_by_path.insert(path.clone(), vt);
                        if was_ingested {
                            ingested.push(path);
                        } else {
                            verified.push(path);
                        }
                        // bug_087: completion-order commit (law 3) —
                        // the success witness raises the committed
                        // floor; driver-serial cumulative, fetch_max
                        // underneath, adapter clamp law untouched.
                        completed_bytes = completed_bytes.saturating_add(nar_size);
                        progress.commit(completed_bytes, "");
                        // merged_bug_003: dedup at ENQUEUE — closure
                        // diamonds under pool contention admit the
                        // shared reference exactly once.
                        for r in references {
                            enqueue_path(
                                &mut visited,
                                &mut frontier,
                                &mut walk_abort,
                                r,
                                PathCell::Reference,
                            );
                        }
                    }
                    PathResolution::Settled {
                        path,
                        cell,
                        trust_refused: t,
                        content_mismatched: c,
                    } => {
                        window_progress.retire(&path);
                        if t {
                            trust_refused.record(path.clone(), tenant_generation);
                        }
                        if c {
                            content_mismatched.record(path.clone(), tenant_generation);
                        }
                        match cell {
                            PathCell::Wanted => missing_wanted.record(path, tenant_generation),
                            PathCell::Reference => {
                                missing_references.record(path, tenant_generation)
                            }
                        }
                    }
                    PathResolution::AbortGrade { path, grade } => {
                        // The aborted path's streamed bytes leave the
                        // fold; a later emission steps back at most to
                        // the committed floor (the truthful direction,
                        // still representable — the spec's MAY).
                        window_progress.retire(&path);
                        walk_abort
                            .get_or_insert_with(Vec::new)
                            .push(AbortEvidence { path, grade });
                    }
                }
            };
        }

        'drive: loop {
            if walk_abort.is_none() {
                // Spawn phase (law 2): frontier order, ≤ F in flight.
                // merged_bug_003: `visited` is marked at ENQUEUE, so
                // frontier membership WITNESSES spawnable work —
                // `!frontier.is_empty()` admits the acquire below only
                // when the slot will be consumed, and a permit is
                // NEVER dropped unused (the dup-only-frontier state is
                // unrepresentable; the closure-walk cap latches at the
                // enqueue sites, and a latched walk never re-enters
                // this phase). gate-share: the SLOT is acquired before
                // the pop — slot waiters hold nothing (the three-gate
                // order).
                while window.len() < fanout && !frontier.is_empty() {
                    let slot = if window.is_empty() {
                        match first_slot.take() {
                            // bug_102: the claim's carried slot seeds
                            // the first width-0 spawn — no FIFO entry
                            // between claim and first spawn.
                            Some(s) => s,
                            // Width-1 baseline invariant (MID-WALK):
                            // blocking-FIFO whenever width 0 + nonempty
                            // frontier (window drained after failed
                            // widening) — with the opportunistic
                            // widening below, the two wakeups jointly
                            // cover every reachable state. Mid-walk the
                            // walk IS the attempt: FIFO gives it the
                            // per-path fair share priced by the wait
                            // facet and charged by the pre-existing
                            // dispatch-deadline contract.
                            None => ctx.path_slots.acquire_baseline().await,
                        }
                    } else {
                        match ctx.path_slots.try_widen() {
                            Some(s) => s,
                            // Widening never blocks; re-attempted on
                            // the next completion event.
                            None => break,
                        }
                    };
                    let (path, cell) = frontier
                        .pop_front()
                        .expect("frontier nonempty by the loop guard");
                    debug_assert!(
                        visited.contains(&path),
                        "frontier membership implies visited (enqueue-dedup)"
                    );
                    // Law 6 (merged_bug_012): the path joins the
                    // driver's window fold at spawn — the future
                    // emits RAW per-path counters and the driver
                    // owns ALL cumulative arithmetic (no base is
                    // captured; a stale-base emission is unwritable).
                    window_progress.admit(&path, fanout);
                    window.push(Box::pin(resolve_path(
                        ctx,
                        claimed,
                        &tenants,
                        &trust_cache,
                        path,
                        cell,
                        tick_tx.clone(),
                        slot,
                    )));
                }
            }
            if walk_abort.is_some() {
                // Law 4: latched. Collect every ALREADY-COMPLETED
                // sibling from the dequeue backlog (non-blocking
                // polls — the batch-simultaneous family included),
                // apply non-abort members normally, then cancel the
                // in-flight rest by drop below.
                use futures_util::FutureExt as _;
                while let Some(Some(res)) = window.next().now_or_never() {
                    apply_resolution!(res);
                }
                break 'drive;
            }
            if window.is_empty() {
                // Frontier drained, nothing in flight: this
                // iteration's walk is complete.
                break 'drive;
            }
            tokio::select! {
                Some(res) = window.next() => {
                    apply_resolution!(res);
                }
                Some(tick) = tick_rx.recv() => {
                    // Law 6 + merged_bug_012: the driver is the SOLE
                    // on_progress caller — commits and provisional
                    // emissions form ONE total order — and the SOLE
                    // owner of cumulative arithmetic: the fold emits
                    // (floor + Σ streamed, floor + Σ declared) over
                    // the in-flight map, so cross-sibling interleave
                    // cannot oscillate the wire pair and a committed
                    // floor passing one path's declared size cannot
                    // render the bar complete. Retired paths' late
                    // ticks are dropped (the zombie guard); the
                    // droppable/non-blocking contract is kept (ticks
                    // arrived try_send).
                    if let Some((done, expected)) = window_progress.tick(completed_bytes, &tick) {
                        progress.emit_provisional(done, expected, &tick.uri);
                    }
                }
            }
        }
        // E-4: the cancelled-sibling window is bounded by F − 1 (the
        // abort itself completed out of a ≤ F window).
        debug_assert!(
            walk_abort.is_none() || window.len() <= fanout.saturating_sub(1),
            "cancelled-sibling window exceeded F-1"
        );
        // Cancellation by drop (law 4): in-flight upstream bodies are
        // torn down with their futures; placeholder drop-guards reap
        // the claims (W-6a); budget reservations ride the hash bytes
        // (WO-R7-1) so a cancelled sibling's budget frees only when
        // its memory does; moka recovers coalesced waiters by RETRY
        // with their own init futures (W-6b pins moka 0.12.15's
        // EnclosingFutureAborted semantics — no adoption). Pending
        // ticks die with the channel — no post-outcome zombie
        // emissions (law 6).
        drop(window);
        drop(tick_tx);
        if let Some(evidence) = walk_abort {
            return fold_abort_evidence(&evidence);
        }
    }

    // ── 5. Outcome ────────────────────────────────────────────────────
    // bug_266: the outcome compiler REFUSES to fold verdict cells
    // older than the final tenant-set generation — the walk drains
    // stale cells at every growth point, so a survivor is a missed
    // drain (a walk bug): surface it as infrastructure failure (the
    // scheduler re-arms), never as a verdict reached on evidence a
    // live tenant was never asked about. The guard is the kernel's
    // (kani: fold_guard accepts iff all-current — K6).
    for (registry, name) in [
        (&missing_wanted, "missing_wanted"),
        (&missing_references, "missing_references"),
        (&trust_refused, "trust_refused"),
        (&content_mismatched, "content_mismatched"),
    ] {
        if let Err(stale) = registry.fold_guard(tenant_generation) {
            return infra_failure(format!(
                "stale verdict cell survived tenant-set growth in {name} \
                 (cell generation {} vs final {}): walk bug — refusing to \
                 fold (bug_266)",
                stale.cell_generation, stale.final_generation
            ));
        }
    }
    if missing_wanted.is_empty() && missing_references.is_empty() {
        info!(
            drv_hash = %claimed.drv_hash,
            ingested = ingested.len(),
            verified = verified.len(),
            "materialization job complete (closure walked, pinned, coverage held)"
        );
        // Signed Q2: per-path verified-tenant sets ride the wire — the
        // scheduler stamps ownership by intersection, never widening.
        let verified_tenants: Vec<materialization_outcome::success::PathTenants> = ingested
            .iter()
            .chain(verified.iter())
            .map(|p| materialization_outcome::success::PathTenants {
                store_path: p.clone(),
                verified_tenant_ids: verified_tenants_by_path
                    .get(p)
                    .map(|ts| ts.iter().map(|t| t.to_string()).collect())
                    .unwrap_or_default(),
            })
            .collect();
        MaterializationOutcome {
            outcome: Some(materialization_outcome::Outcome::Success(
                materialization_outcome::Success {
                    ingested_paths: ingested,
                    verified_paths: verified,
                    verified_tenants,
                },
            )),
        }
    } else {
        // Confirmed-404 paths: the Unobtainable verdict carries the
        // missing WANTED cell, the missing REFERENCE cell
        // (merged_bug_193 — the consumer's moot arm requires both
        // empty), and what WAS obtained.
        let mut covered = ingested;
        covered.extend(verified);
        warn!(
            drv_hash = %claimed.drv_hash,
            missing_wanted = missing_wanted.len(),
            missing_references = missing_references.len(),
            covered = covered.len(),
            "materialization job unobtainable (confirmed-absent paths)"
        );
        let mut cause = format!(
            "{} wanted / {} reference path(s) confirmed absent at every \
             configured upstream",
            missing_wanted.len(),
            missing_references.len()
        );
        if !trust_refused.is_empty() {
            // merged_bug_005: the refusal is actionable configuration
            // feedback — name it instead of letting it read as a
            // generic miss.
            cause.push_str(&format!(
                "; {} of them present upstream but no narinfo signature \
                 verified against trusted_keys (rotated or mistyped key?)",
                trust_refused.len()
            ));
        }
        if !content_mismatched.is_empty() {
            // merged_bug_046: the content disagreement is its own
            // actionable cause — never folded into the trust wording
            // (a key rotation will not fix disagreeing bytes).
            cause.push_str(&format!(
                "; {} of them present upstream but claiming different \
                 bytes than the stored row (content disagreement at the \
                 dedup arm)",
                content_mismatched.len()
            ));
        }
        // bug_084: BOTH wire refusal fields are minted by the ONE
        // constructor below — the cause string above names the axes
        // for humans; the typed pair is what the settlement consumes.
        let (refusal, trust_refused_echo) = refusal_wire(&trust_refused, &content_mismatched);
        MaterializationOutcome {
            outcome: Some(materialization_outcome::Outcome::Unobtainable(
                materialization_outcome::Unobtainable {
                    cause,
                    missing_paths: missing_wanted.paths().map(str::to_owned).collect(),
                    verified_paths: covered,
                    missing_reference_paths: missing_references
                        .paths()
                        .map(str::to_owned)
                        .collect(),
                    trust_refused: trust_refused_echo,
                    refusal,
                },
            )),
        }
    }
}

/// The SOLE writer of the Unobtainable wire refusal pair (bug_084):
/// field 6 (`refusal`, the closed [`UnobtainableRefusal`] alphabet)
/// and field 5 (`trust_refused`, the decode-ignored coherence echo —
/// SIGNED Q6, bughunt-5 §5-S 2026-06-09: --wipe rollout, no skew
/// lane) derive together from the walk's two refusal cell-sets, so an
/// incoherent pair (`refusal: CONTENT` with `trust_refused: true`,
/// a shape the walk cannot observe) is unwritable — coherence by
/// construction, never by review.
///
/// [`UnobtainableRefusal`]: rio_proto::types::UnobtainableRefusal
fn refusal_wire(
    trust_refused: &rio_evidence_kernel::outcome::GenStampedCells,
    content_mismatched: &rio_evidence_kernel::outcome::GenStampedCells,
) -> (i32, bool) {
    use rio_proto::types::UnobtainableRefusal;
    let refusal = match (!trust_refused.is_empty(), !content_mismatched.is_empty()) {
        (false, false) => UnobtainableRefusal::Unspecified,
        (true, false) => UnobtainableRefusal::Trust,
        (false, true) => UnobtainableRefusal::Content,
        (true, true) => UnobtainableRefusal::TrustAndContent,
    };
    (
        refusal.into(),
        matches!(
            refusal,
            UnobtainableRefusal::Trust | UnobtainableRefusal::TrustAndContent
        ),
    )
}

/// Which cell a frontier path belongs to (merged_bug_193): a
/// live-wanted SEED routes a confirmed miss to
/// `Unobtainable.missing_paths`; a narinfo/local-row REFERENCE
/// extension routes to `missing_reference_paths` — the consumer's
/// moot-completion arm requires both empty.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PathCell {
    Wanted,
    Reference,
}

// r[impl store.materialize.path-fold+1]
/// The CLOSED per-path evidence object (path-fold law 1, live_047/R-C):
/// a path future returns exactly one of these and NEVER mutates job
/// state or returns a job-level outcome. The driver's apply is a total
/// match over this alphabet (zero `_ =>` — the §5.1 variant-totality
/// census; a new variant fails the build at the apply site).
enum PathResolution {
    /// Served: pinned (pin-at-ingest already ran inside the future —
    /// I-5 holds before the driver can count it), persisted/visible;
    /// the driver commits the floor, records the verified tenants, and
    /// extends the frontier with `references` (already self-filtered).
    Served {
        path: String,
        nar_size: u64,
        references: Vec<String>,
        verified_tenants: Vec<Uuid>,
        /// true = substitute-hit (`ingested_paths`); false =
        /// local-present (`verified_paths`).
        ingested: bool,
    },
    /// Settled uncharged (the walk continues): the driver records the
    /// generation-stamped cells — the missing cell per `cell`, plus
    /// the trust/content cause registries when flagged.
    Settled {
        path: String,
        cell: PathCell,
        trust_refused: bool,
        content_mismatched: bool,
    },
    /// Abort-grade: job-level disposition evidence. The driver latches
    /// (no further spawns), folds the already-completed backlog by
    /// tier, cancels in-flight siblings by drop, and compiles the job
    /// outcome from the completed-evidence multiset.
    AbortGrade {
        path: String,
        grade: AbortDisposition,
    },
}

/// The two abort grades a path can complete with. Tier precedence is
/// NOT defined here — [`AbortDisposition::tier`] maps through the
/// kernel's [`disposition_tier`] table, the single executable
/// precedence source both the tenant and upstream folds already
/// consume (merged_bug_210; charge = 0 ≺ transient = 1).
///
/// [`disposition_tier`]: rio_evidence_kernel::outcome::disposition_tier
enum AbortDisposition {
    /// Charge-grade: reaches the charge ladder as InfraFailure.
    Charge { detail: String },
    /// Transient: closes UNCHARGED as RetryLater (a 429 wave must
    /// never park a healthy job), carrying the wire class label and
    /// the upstream's back-off advice.
    Transient {
        class: &'static str,
        detail: String,
        retry_after: Option<Duration>,
    },
}

impl AbortDisposition {
    /// Precedence tier via the kernel's single source.
    fn tier(&self) -> u8 {
        use rio_evidence_kernel::outcome::{FailureDisposition, disposition_tier};
        match self {
            AbortDisposition::Charge { .. } => disposition_tier(FailureDisposition::ChargeInfra),
            AbortDisposition::Transient { .. } => {
                disposition_tier(FailureDisposition::RetryUncharged)
            }
        }
    }
}

/// One completed abort-grade resolution in the latch backlog.
struct AbortEvidence {
    path: String,
    grade: AbortDisposition,
}

// r[impl store.materialize.path-fold+1]
/// THE path-axis abort fold (path-fold law 4): the job outcome is a
/// pure function of the completed abort-evidence MULTISET, never of
/// arrival order within it — input-order invariant by construction
/// (W-1a proptest-swept):
///
/// - winning tier = min [`AbortDisposition::tier`] (charge dominates
///   transient — the kernel table, same source as the tenant fold);
/// - the wire representative is the LEXICOGRAPHICALLY-FIRST path
///   within the winning tier (never first-dequeued, so no
///   schedule-dependent detail rides the wire);
/// - the transient tier carries the MAX `retry_after` across ALL
///   completed transient abort-grades (the tenant-fold
///   max-across-tenants precedent one axis up).
fn fold_abort_evidence(evidence: &[AbortEvidence]) -> MaterializationOutcome {
    debug_assert!(!evidence.is_empty(), "fold over empty abort evidence");
    let win_tier = evidence
        .iter()
        .map(|e| e.grade.tier())
        .min()
        .expect("nonempty");
    let rep = evidence
        .iter()
        .filter(|e| e.grade.tier() == win_tier)
        .min_by(|a, b| a.path.cmp(&b.path))
        .expect("winning tier nonempty");
    let max_retry = evidence
        .iter()
        .filter_map(|e| match &e.grade {
            AbortDisposition::Transient { retry_after, .. } => *retry_after,
            AbortDisposition::Charge { .. } => None,
        })
        .max();
    match &rep.grade {
        AbortDisposition::Charge { detail } => infra_failure(detail.clone()),
        AbortDisposition::Transient { class, detail, .. } => MaterializationOutcome {
            outcome: Some(materialization_outcome::Outcome::RetryLater(
                materialization_outcome::RetryLater {
                    detail: detail.clone(),
                    retry_after_secs: max_retry.map(|d| d.as_secs()).unwrap_or(0),
                    class: (*class).to_string(),
                },
            )),
        },
    }
}

/// A per-path progress EVENT (path-fold law 6): path futures never
/// call the progress adapter — they `try_send` (droppable, the relay
/// contract) RAW per-path counters onto the driver's stream, and the
/// DRIVER is the sole `on_progress` caller AND the sole owner of
/// cumulative arithmetic (merged_bug_012: the WindowProgress fold), so
/// commits and provisional emissions form one total order and the
/// per-call floor MUST of `store.materialize.progress-monotone+1` is
/// enforceable trace-wide (post-outcome zombie emissions are
/// impossible — forwarding stops at outcome; post-RETIRE zombie ticks
/// are dropped by the fold's membership).
struct ProgressTick {
    /// The emitting path — the fold's map key (merged_bug_012: ticks
    /// are RAW per-path events; a spawn-captured base is no longer
    /// writable because there is no base field to fill).
    path: String,
    /// Bytes of this path's CURRENT fetch streamed so far (restarts
    /// at zero on a stall failover — the licensed step-back).
    streamed: u64,
    /// The fetch's declared total (NarSize from the serving narinfo).
    declared: u64,
    uri: String,
}

/// Driver-owned per-iteration window progress fold (merged_bug_012):
/// at width > 1 only the component with a total order over emissions
/// may compute cumulative display values, and the driver IS that
/// component (path-fold law 6 — sole `on_progress` caller). Paths are
/// ADMITTED at spawn, updated per raw tick, and RETIRED on every apply
/// arm; the aggregate emission is `(floor + Σ streamed, floor + Σ
/// declared)` over the in-flight map. Late ticks for retired paths
/// find no entry and are dropped — the zombie guard. The in-flight map
/// is window-bounded (≤ F entries — law 2).
#[derive(Default)]
struct WindowProgress {
    in_flight: HashMap<String, (u64, u64)>,
}

impl WindowProgress {
    /// Register a path at SPAWN (membership is the zombie guard's
    /// authority: tick() updates only existing entries). The map is
    /// window-bounded — the R17 memory envelope, asserted here.
    fn admit(&mut self, path: &str, fanout: usize) {
        debug_assert!(
            self.in_flight.len() < fanout,
            "in-flight fold exceeds the window bound (law 2)"
        );
        self.in_flight.insert(path.to_string(), (0, 0));
    }

    /// Fold one raw tick; returns the aggregate `(done, expected)`
    /// over `floor`, or None for a retired path's late tick.
    fn tick(&mut self, floor: u64, t: &ProgressTick) -> Option<(u64, u64)> {
        let entry = self.in_flight.get_mut(&t.path)?;
        *entry = (t.streamed, t.declared);
        let (sum_streamed, sum_declared) = self
            .in_flight
            .values()
            .fold((0u64, 0u64), |(s, d), (ps, pd)| {
                (s.saturating_add(*ps), d.saturating_add(*pd))
            });
        Some((
            floor.saturating_add(sum_streamed),
            floor.saturating_add(sum_declared),
        ))
    }

    /// Drop a path from the fold (every apply arm — Served, Settled,
    /// AbortGrade). Subsequent ticks from it are ignored.
    fn retire(&mut self, path: &str) {
        self.in_flight.remove(path);
    }
}

/// Per-iteration tick-queue depth. Sized for display traffic (ticks
/// are throttled to SUBSTITUTE_PROGRESS_INTERVAL_BYTES upstream);
/// overflow drops the tick — progress is droppable by contract.
const PROGRESS_TICK_QUEUE: usize = 64;

// r[impl store.materialize.gate-share+1]
/// The pod-wide executor path-slot pool (live_047/R-C WO-R7-3): ONE
/// fair-FIFO semaphore of `P = effective_admission_cap / 2` permits
/// (`derive_executor_path_slots` over the SAME effective value main.rs
/// constructs the admission gate from — override included). A path
/// future exists only while holding a slot, so executor-held admission
/// permits ≤ in-flight path futures ≤ held slots ≤ P — the d1f18610d
/// "executor caps at half the gate" invariant made STRUCTURAL,
/// independent of `path_fanout` and `executor_concurrency`.
///
/// **Yield law (normative — the equilibrium rows ride on it):**
/// widening MUST be non-preferential: a freed slot is assigned to
/// QUEUED baseline waiters BEFORE any `try_acquire` can observe it.
/// tokio's batch semaphore provides exactly this (releases assign
/// permits to the wait queue first; `try_acquire` CASes only the
/// queue-empty leftovers — vendored 1.52.3 `batch_semaphore.rs`).
/// REJECTED shapes, named so an implementation cannot satisfy the
/// permit CEILING while losing the property: a split counter, a
/// second extras-semaphore, any non-queue-respecting fast path —
/// each lets widened walks re-capture freed slots indefinitely,
/// starving queued walks.
///
/// Gate order (the §3.2 no-deadlock record, three-gate form): **slot ≺
/// admission ≺ budget** — slot waiters hold nothing; admission waiters
/// may hold only a slot; budget waiters hold both but never wait on
/// either. The wait graph stays acyclic.
#[derive(Clone)]
pub struct PathSlotPool {
    slots: std::sync::Arc<tokio::sync::Semaphore>,
    capacity: usize,
    /// Test-only hook invoked between `publish_in_use`'s read and its
    /// set — the lost-update interleave seam for the gauge red. `None`
    /// everywhere outside the owning test.
    #[cfg(test)]
    publish_gate: Option<std::sync::Arc<dyn Fn() + Send + Sync>>,
}

impl PathSlotPool {
    /// New pool of `capacity` slots (main.rs passes
    /// [`crate::config::derive_executor_path_slots`] of the effective
    /// admission cap; tests size it directly).
    pub fn new(capacity: usize) -> Self {
        Self {
            slots: std::sync::Arc::new(tokio::sync::Semaphore::new(capacity)),
            capacity,
            #[cfg(test)]
            publish_gate: None,
        }
    }

    /// Install the read/set interleave hook (test-only).
    #[cfg(test)]
    fn with_publish_gate(mut self, gate: std::sync::Arc<dyn Fn() + Send + Sync>) -> Self {
        self.publish_gate = Some(gate);
        self
    }

    /// Configured pool size (the boot-warn comparand).
    pub fn capacity(&self) -> usize {
        self.capacity
    }

    /// Width-1 BASELINE acquire — blocking-FIFO. Used whenever a walk
    /// holds ZERO slots with a nonempty frontier: at job start AND any
    /// time the window drains to zero after a failed widening (the
    /// width-1 baseline invariant — the first-path-only form has a
    /// permanent mid-walk stall: the fair semaphore hands a freed slot
    /// to a queued waiter before any `try_acquire` observes it, and a
    /// width-0 walk has no completion event to wake on).
    ///
    /// Module-owned BOTH-EDGES instrumentation (the admission-gate
    /// pattern): the queued-baseline-waiters gauge rises on queue
    /// entry and falls on exit (including cancellation — the guard),
    /// and the wait-age histogram is the §4.2 TRANSIENT-queuing
    /// tripwire (reachable at helm whenever n×F > P; the boot warn
    /// covers only the PERMANENT n > P regime).
    async fn acquire_baseline(&self) -> PathSlot {
        struct WaiterGuard;
        impl Drop for WaiterGuard {
            fn drop(&mut self) {
                metrics::gauge!("rio_store_executor_path_slot_baseline_waiters").decrement(1.0);
            }
        }
        metrics::gauge!("rio_store_executor_path_slot_baseline_waiters").increment(1.0);
        let guard = WaiterGuard;
        let started = std::time::Instant::now();
        let permit = self
            .slots
            .clone()
            .acquire_owned()
            .await
            .expect("path-slot pool is never closed");
        drop(guard);
        metrics::histogram!("rio_store_executor_path_slot_baseline_wait_seconds")
            .record(started.elapsed().as_secs_f64());
        self.publish_in_use();
        PathSlot {
            permit: Some(permit),
            pool: self.clone(),
        }
    }

    /// Opportunistic WIDENING (width ≥ 1 → width + 1): never blocks
    /// and never overtakes queued baseline waiters (`try_acquire`
    /// observes only queue-empty leftovers — the yield law above).
    fn try_widen(&self) -> Option<PathSlot> {
        let permit = self.slots.clone().try_acquire_owned().ok()?;
        self.publish_in_use();
        Some(PathSlot {
            permit: Some(permit),
            pool: self.clone(),
        })
    }

    /// Non-blocking CLAIM ADMISSION (bug_102 — the slot ≺ claim gate):
    /// a worker obtains its first path slot BEFORE the claiming pull;
    /// a worker with no slot headroom mints NO claim — the job stays
    /// scheduler-listed, claimable by any pod with headroom (strictly
    /// better placement than queueing inside an open attempt window).
    /// Leftover-only try-acquire per the yield law: claim admission
    /// never overtakes queued mid-walk baseline waiters, so
    /// finish-started-work-first falls out of the existing semaphore
    /// discipline.
    pub fn try_admit_claim(&self) -> Option<ClaimAdmission> {
        let permit = self.slots.clone().try_acquire_owned().ok()?;
        self.publish_in_use();
        Some(ClaimAdmission {
            first_slot: PathSlot {
                permit: Some(permit),
                pool: self.clone(),
            },
        })
    }

    /// Republish the in-use gauge from the pool's current truth.
    fn publish_in_use(&self) {
        let in_use = self.capacity.saturating_sub(self.slots.available_permits());
        // Test-only interleave seam: parks THIS publisher between its
        // read (above) and its set (below) so a sibling's full
        // read-then-set can land in the window.
        #[cfg(test)]
        if let Some(gate) = &self.publish_gate {
            gate();
        }
        metrics::gauge!("rio_store_executor_path_slots_in_use").set(in_use as f64);
    }
}

/// A held path slot: releasing (Drop) returns the permit FIRST, then
/// republishes the in-use gauge from post-release truth (the
/// AdmissionPermit both-edges pattern — bug_245: an acquire-only gauge
/// freezes at the high-water on the scrape surface). Rides INSIDE the
/// path future, so cancellation-by-drop releases the slot with it.
struct PathSlot {
    permit: Option<tokio::sync::OwnedSemaphorePermit>,
    pool: PathSlotPool,
}

/// The CLAIM gate's typed admission (bug_102): holding one is the
/// proof that this worker held a path slot BEFORE its claiming pull —
/// the gate order is **slot ≺ claim ≺ admission ≺ budget**, so an
/// attempt window opens only on admitted work and "slot waiters hold
/// nothing" is true from the claim onward (the establishment sweep's
/// pricing premise — window time is slot-held work or crash — holds
/// again). Minted ONLY by [`PathSlotPool::try_admit_claim`];
/// [`execute_job`]/[`execute_job_with_progress`] DEMAND one, so
/// claim-without-slot does not typecheck. The carried slot seeds
/// iteration 1's first width-0 spawn without touching the baseline
/// FIFO; a walk that never spawns drops it — slot returned.
pub struct ClaimAdmission {
    first_slot: PathSlot,
}

impl Drop for PathSlot {
    fn drop(&mut self) {
        drop(self.permit.take());
        self.pool.publish_in_use();
    }
}

// r[impl store.materialize.path-fold+1]
/// Resolve ONE path against live state: the evidence-returning path
/// future (path-fold law 1). The body is the serial walk's per-path
/// block verbatim — local probe, the per-tenant substitute loop
/// (tenant-fold law untouched: cells, `AbortRaced`, post-loop fold),
/// the AllCleanMiss probe leg — with every in-loop job-level return
/// replaced by a closed [`PathResolution`].
#[allow(clippy::too_many_arguments)]
async fn resolve_path(
    ctx: &ExecutorContext,
    claimed: &ClaimedJob,
    tenants: &[Uuid],
    trust_cache: &SharedTrustCache,
    path: String,
    cell: PathCell,
    ticks: tokio::sync::mpsc::Sender<ProgressTick>,
    // gate-share: the slot spans the WHOLE path future (local probe +
    // tenant loop + miss-probe leg — coarser but single-region; the
    // pool is a ceiling, not a throughput promise) and releases on
    // completion OR cancellation-by-drop.
    _slot: PathSlot,
) -> PathResolution {
    // bug_042: the local-presence probe is a VERDICT input, so its
    // error is charging evidence (a PG blip is never absence).
    // bug_115/bug_139: Present requires the visibility witness.
    // bug_073: no job-wide guard — sibling probes run concurrently;
    // the cache locks internally per operation.
    let probed = probe_local(ctx, &path, tenants, trust_cache).await;
    let local_witness = match probed {
        Ok(LocalPresence::Present(visible, info)) => {
            // r[impl sched.materialize.pinning]
            // Pin-at-ingest BEFORE the Served evidence exists (I-5):
            // the driver applies Served only with the pin landed.
            if let Err(e) = pin_materialized_path(ctx, claimed, &path).await {
                return PathResolution::AbortGrade {
                    grade: AbortDisposition::Charge {
                        detail: format!("pin-at-ingest failed for {path}: {e}"),
                    },
                    path,
                };
            }
            let references = info
                .references
                .iter()
                .map(|r| r.as_str().to_string())
                .filter(|r| *r != path)
                .collect();
            return PathResolution::Served {
                nar_size: info.nar_size,
                verified_tenants: visible.tenants().to_vec(),
                references,
                ingested: false,
                path,
            };
        }
        Ok(LocalPresence::Absent(w)) => w,
        Err(e) => {
            return PathResolution::AbortGrade {
                grade: AbortDisposition::Charge {
                    detail: format!("local presence probe failed for {path}: {e}"),
                },
                path,
            };
        }
    };

    // merged_bug_195 + path-fold law 6 + merged_bug_012: per-path
    // progress through the substitute body fetch rides the driver's
    // event stream as RAW per-path counters — the future reports only
    // its own (streamed, declared); the driver's WindowProgress fold
    // owns the cumulative arithmetic and the adapter clamp does the
    // rest. (The old spawn-captured base is deleted: at width > 1 it
    // oscillated the wire pair by the siblings' stream gap and
    // rendered a false 100% once commits passed a still-streaming
    // path's base + declared.)
    let path_for_ticks = path.clone();
    let per_path_progress = move |streamed: u64, declared: u64, uri: &str| {
        let _ = ticks.try_send(ProgressTick {
            path: path_for_ticks.clone(),
            streamed,
            declared,
            uri: uri.to_string(),
        });
    };

    // merged_bug_028 / owner Q2 + merged_bug_133: try EVERY interested
    // tenant's upstream view until one serves the path. The loop body
    // ONLY pushes evidence cells (a hit breaks) — ALL failure
    // dispositions exit at the kernel fold below (tenant-fold law,
    // verbatim inside the path future).
    let mut hit: Option<(Uuid, Box<rio_proto::validated::ValidatedPathInfo>)> = None;
    let mut cells = TenantAttemptCells::new();
    let mut cell_msgs: Vec<(&'static str, String)> = Vec::with_capacity(tenants.len());
    for &tenant_id in tenants {
        match ctx
            .substituter
            .try_substitute_with_progress(tenant_id, &path, &per_path_progress)
            .await
        {
            Ok(Some(path_info)) => {
                hit = Some((tenant_id, Box::new(path_info)));
                break;
            }
            Ok(None) => {
                cells.record_clean_miss();
                cell_msgs.push(("", String::new()));
            }
            Err(e) => {
                // merged_bug_178/bug_194/merged_bug_188: total
                // classification + the one message chokepoint + the
                // kernel loop control (Raced aborts the tenant axis).
                let (class, retry_after) = crate::substitute::substitute_error_evidence(&e);
                let (label, msg) = substitute_cell_message(&path, class, &e);
                cell_msgs.push((label, msg));
                match cells.record_failure(class, retry_after) {
                    LoopControl::Continue => {}
                    LoopControl::AbortRaced => break,
                }
            }
        }
    }
    match hit {
        Some((serving_tenant, path_info)) => {
            // r[impl sched.materialize.pinning]
            // Pin-at-ingest (design §5.1) before Served evidence.
            if let Err(e) = pin_materialized_path(ctx, claimed, &path).await {
                return PathResolution::AbortGrade {
                    grade: AbortDisposition::Charge {
                        detail: format!("pin-at-ingest failed for {path}: {e}"),
                    },
                    path,
                };
            }
            // Signed Q2: serving tenant verified; remaining interested
            // tenants re-checked against the now-local row.
            let mut vt: Vec<Uuid> = vec![serving_tenant];
            if tenants.len() > 1
                && let Ok(Some(local)) = crate::metadata::query_path_info(&ctx.pool, &path).await
            {
                let signer = ctx.substituter.tenant_signer();
                for &other in tenants.iter().filter(|t| **t != serving_tenant) {
                    if let Ok(Some(v)) =
                        visible_to_tenant(&ctx.pool, signer, Some(other), &local, trust_cache).await
                    {
                        for t in v.tenants() {
                            if !vt.contains(t) {
                                vt.push(*t);
                            }
                        }
                    }
                }
            }
            let references = path_info
                .references
                .iter()
                .map(|r| r.as_str().to_string())
                .filter(|r| *r != path)
                .collect();
            PathResolution::Served {
                nar_size: path_info.nar_size,
                verified_tenants: vt,
                references,
                ingested: true,
                path,
            }
        }
        None => match cells.fold() {
            TenantAttemptsVerdict::ChargeInfra { idx } => PathResolution::AbortGrade {
                grade: AbortDisposition::Charge {
                    detail: cell_msgs[idx].1.clone(),
                },
                path,
            },
            TenantAttemptsVerdict::RetryTransient { idx, max } => {
                let (label, detail) = &cell_msgs[idx];
                info!(path = %path, class = label,
                      "transient substitute failure; reporting retry-later");
                PathResolution::AbortGrade {
                    grade: AbortDisposition::Transient {
                        class: label,
                        detail: detail.clone(),
                        retry_after: max,
                    },
                    path,
                }
            }
            TenantAttemptsVerdict::UntrustedPresent { idx } => {
                // merged_bug_005: present-but-untrusted settles
                // uncharged WITHOUT the HEAD confirmation; the
                // local-miss witness anchors the verdict.
                let _witness: LocalMiss = local_witness;
                let (_, detail) = &cell_msgs[idx];
                warn!(path = %path, detail = %detail,
                      "path present upstream but signature-untrusted; \
                       settling unobtainable (uncharged)");
                PathResolution::Settled {
                    path,
                    cell,
                    trust_refused: true,
                    content_mismatched: false,
                }
            }
            TenantAttemptsVerdict::ContentMismatch { idx } => {
                // merged_bug_046: stored-row content disagreement
                // settles uncharged WITHOUT the HEAD confirmation.
                let _witness: LocalMiss = local_witness;
                let (_, detail) = &cell_msgs[idx];
                warn!(path = %path, detail = %detail,
                      "path present upstream with disagreeing content; \
                       settling unobtainable (uncharged)");
                PathResolution::Settled {
                    path,
                    cell,
                    trust_refused: false,
                    content_mismatched: true,
                }
            }
            TenantAttemptsVerdict::AllCleanMiss => {
                // merged_bug_028/bug_042: the miss verdict additionally
                // requires HEAD-probe confirmation under EVERY tenant,
                // riding the SAME cells + fold (merged_bug_133).
                let mut probe_cells = TenantAttemptCells::new();
                let mut probe_msgs: Vec<String> = Vec::with_capacity(tenants.len());
                for &tenant_id in tenants {
                    match probe_miss(ctx, tenant_id, &path).await {
                        MissProbe::Confirmed => {
                            probe_cells.record_clean_miss();
                            probe_msgs.push(String::new());
                        }
                        MissProbe::Failed {
                            class,
                            retry_after,
                            detail,
                        } => {
                            // bug_295/bug_194/merged_bug_188: class
                            // congruence per CLASS, the shared message
                            // chokepoint, the same loop control.
                            probe_msgs.push(substitute_cell_message(&path, class, &detail).1);
                            match probe_cells.record_failure(class, retry_after) {
                                LoopControl::Continue => {}
                                LoopControl::AbortRaced => break,
                            }
                        }
                    }
                }
                let mut trust_flag = false;
                let mut content_flag = false;
                match probe_cells.fold() {
                    TenantAttemptsVerdict::ChargeInfra { idx } => {
                        return PathResolution::AbortGrade {
                            grade: AbortDisposition::Charge {
                                detail: probe_msgs[idx].clone(),
                            },
                            path,
                        };
                    }
                    TenantAttemptsVerdict::RetryTransient { idx, max } => {
                        // bug_295: probe-leg rate-limit waves close
                        // UNCHARGED with the advice riding the deferral.
                        info!(path = %path, class = "rate_limited",
                              "transient probe failure; reporting retry-later");
                        return PathResolution::AbortGrade {
                            grade: AbortDisposition::Transient {
                                class: "rate_limited",
                                detail: probe_msgs[idx].clone(),
                                retry_after: max,
                            },
                            path,
                        };
                    }
                    TenantAttemptsVerdict::UntrustedPresent { .. } => {
                        // Unreachable while the HEAD probe is sig-blind;
                        // a future sig-aware probe's refusal settles
                        // with the trust cause recorded.
                        trust_flag = true;
                    }
                    TenantAttemptsVerdict::ContentMismatch { .. } => {
                        // Unreachable while the HEAD probe is
                        // content-blind (merged_bug_046); mirrors the
                        // trust arm.
                        content_flag = true;
                    }
                    TenantAttemptsVerdict::AllCleanMiss => {}
                }
                let _witness: LocalMiss = local_witness;
                debug!(path = %path, cell = ?cell, tenants = tenants.len(),
                       "path confirmed absent under every interested tenant (and locally)");
                PathResolution::Settled {
                    path,
                    cell,
                    trust_refused: trust_flag,
                    content_mismatched: content_flag,
                }
            }
        },
    }
}

/// Witness that the LOCAL presence probe ran and answered "absent
/// under every interested tenant's view" (bug_042 + bug_115): either
/// no complete local manifest exists, or one exists but the
/// sig-visibility gate hides it from EVERY interested tenant (presence
/// is a per-tenant fact — owner Q2). Not constructible outside
/// [`probe_local`] — a `ConfirmedAbsent`-shaped verdict therefore
/// PROVES the path is neither locally servable nor upstream; upstream
/// absence alone no longer typechecks as a missing-path verdict.
struct LocalMiss(());

/// The local-presence answer for one walk path.
enum LocalPresence {
    /// A complete local manifest exists AND it is visible to at least
    /// one interested tenant (bug_115: the witness is a required
    /// field, so the tenant-blind "physically present ⇒ serve" arm is
    /// uncompilable) — the full row (references, nar_size) drives pin
    /// + frontier extension without upstream.
    Present(TenantVisible, Box<rio_proto::validated::ValidatedPathInfo>),
    /// No locally-servable manifest: absent, or gate-hidden from every
    /// interested tenant (the row degrades to the per-tenant
    /// substitute lane — a tenant whose upstream serves the path
    /// re-fetches it under its OWN trust view).
    Absent(LocalMiss),
}

/// The walk's local-presence probe. The error PROPAGATES (bug_042):
/// a PG blip is infrastructure trouble, never evidence of absence.
///
/// ── SIGNED 2026-06-07 (owner, bughunt-3 fix-wave sec.5-S Q2) ──
/// Per-tenant verified stamping: materialization Success evidence is
/// stamped ONLY for tenants whose view validated each path — the
/// walk's visibility witness ([`TenantVisible`], a set-carrier) or an
/// own-upstream substitute hit plus the post-ingest re-check under
/// the remaining interested tenants. The verified-tenant sets travel
/// on the wire (`MaterializationOutcome.Success.verified_tenants`,
/// build_types.proto field 3, R10 — S5 reviewer-of-record) and the
/// scheduler's stamp funnel (`StampProvenance::WalkVerified`)
/// INTERSECTS them with the attributed tenants. Unverified interested
/// tenants keep their interest open for the next walk. All six
/// scheduler stamp-producer sites route through the typed witness; a
/// witness-less stamp does not compile.
/// ──────────────────────────────────────────────────────────────────
///
/// bug_115: a physically-present row is servable only if the
/// sig-visibility verdict (the SAME body as the gRPC read gates —
/// [`crate::visibility::visible_to_tenant`]) passes for at least one
/// interested tenant. A gate-hidden row (substitution-only signed by
/// keys none of the interested tenants trust, or another tenant's
/// built output per I-217) answers Absent, so the walk falls through
/// to the per-tenant substitute lane instead of laundering the row
/// into the interested tenants' durable ownership
/// (`upsert_path_tenants_for_batch` stamps every interested build's
/// tenant over the job's verified paths on Success).
// r[impl store.materialize.local-visibility]
async fn probe_local(
    ctx: &ExecutorContext,
    store_path: &str,
    tenants: &[Uuid],
    trust_cache: &SharedTrustCache,
) -> Result<LocalPresence, crate::metadata::MetadataError> {
    // Test-only rendezvous seam (see `ExecutorContext::probe_rendezvous`):
    // sized at F by the probe-concurrency red — completes only when F
    // sibling probes overlap in time.
    #[cfg(test)]
    if let Some(rendezvous) = &ctx.probe_rendezvous {
        rendezvous.wait().await;
    }
    let Some(info) = crate::metadata::query_path_info(&ctx.pool, store_path).await? else {
        return Ok(LocalPresence::Absent(LocalMiss(())));
    };
    let signer = ctx.substituter.tenant_signer();
    // bug_139 / signed Q2: consult EVERY interested tenant and carry
    // the full visible set in the witness — the pre-fix first-visible
    // break minted an existence witness that downstream stamping
    // widened to all-tenant ownership (the exists-gate/forall-stamp
    // laundering).
    let mut witness: Option<TenantVisible> = None;
    for &tid in tenants {
        if let Some(visible) =
            visible_to_tenant(&ctx.pool, signer, Some(tid), &info, trust_cache).await?
        {
            match &mut witness {
                None => witness = Some(visible),
                Some(w) => w.merge(visible),
            }
        }
    }
    if let Some(visible) = witness {
        return Ok(LocalPresence::Present(visible, Box::new(info)));
    }
    debug!(
        path = %store_path,
        tenants = tenants.len(),
        "local row is gate-hidden from every interested tenant; \
         degrading to the per-tenant substitute lane"
    );
    Ok(LocalPresence::Absent(LocalMiss(())))
}

/// Shorthand for the InfraFailure outcome.
fn infra_failure(detail: impl Into<String>) -> MaterializationOutcome {
    MaterializationOutcome {
        outcome: Some(materialization_outcome::Outcome::InfraFailure(
            materialization_outcome::InfraFailure {
                detail: detail.into(),
            },
        )),
    }
}

/// AS-4 / PDQ-8 tenant re-resolution against live interest — PLURAL
/// (merged_bug_028 / owner Q2 2026-06-03: the executor may satisfy a
/// job through ANY interested tenant's upstreams; a job fails only
/// when NO interested tenant can obtain).
///
/// The recorded creating-build tenant ([`ClaimedJob::tenant_hint`]) is
/// a preference honored only while a live interested build still
/// carries it (it sorts FIRST); the rest follow in tenant_id order
/// (deterministic). Empty = no resolvable tenant context (the caller
/// reports InfraFailure). A terminal creating build's tenant may have
/// been deleted (its `tenant_upstreams` cascade-removed) or carry
/// sig-trust the live interest cannot see — re-resolution is the
/// authority, never the recorded value.
async fn resolve_tenants(
    ctx: &ExecutorContext,
    claimed: &ClaimedJob,
) -> Result<Vec<Uuid>, sqlx::Error> {
    // 086: live interest derives from build_derivations membership
    // (the live_wanted_interest view) — a row-less live build's tenant
    // is discoverable (merged_bug_176).
    let mut live: Vec<Uuid> = sqlx::query_scalar(
        "SELECT DISTINCT i.tenant_id \
           FROM materialization_jobs j \
           JOIN live_wanted_interest i USING (derivation_id) \
          WHERE j.drv_hash = $1 \
            AND i.tenant_id IS NOT NULL \
          ORDER BY i.tenant_id",
    )
    .bind(&claimed.drv_hash)
    .fetch_all(&ctx.pool)
    .await?;

    if let Some(hint) = claimed.tenant_hint
        && let Some(pos) = live.iter().position(|t| *t == hint)
    {
        live.remove(pos);
        live.insert(0, hint);
    }
    Ok(live)
}

/// The live wanted set for the claimed derivation, as store paths.
///
/// Reads `build_wanted_outputs` rows of LIVE builds (the
/// materialization_interest view's predicate), unions the wanted
/// output names with the `'{}'` = all saturation convention (the 062
/// convention; the scheduler-side single home is db/wanted.rs —
/// cross-reference), and maps names → paths through the derivation
/// row's parallel `output_names` ↔ `expected_output_paths` arrays.
async fn live_wanted_paths(
    ctx: &ExecutorContext,
    claimed: &ClaimedJob,
) -> Result<Vec<String>, sqlx::Error> {
    let rows: Vec<(Vec<String>, Vec<String>, Vec<String>)> =
        sqlx::query_as(rio_migrations::sql::LIVE_WANTED_NAME_ROWS_BY_DRV_SQL)
            .bind(&claimed.drv_hash)
            .fetch_all(&ctx.pool)
            .await?;

    // Name-level fold: THE one fold body (merged_bug_059 — the
    // scheduler's in-memory view and SQL twin route through the same
    // function, so the width definition cannot fork at the fold).
    let mut paths: Vec<String> = match rows.first() {
        None => Vec::new(),
        Some((output_names, expected_paths, _)) => {
            let union = rio_common::wanted_outputs::saturating_wanted_union(
                rows.iter().map(|(_, _, wanted)| wanted.as_slice()),
            );
            let all = matches!(&union, Some(v) if v.is_empty());
            let names = union.unwrap_or_default();
            output_names
                .iter()
                .zip(expected_paths.iter())
                .filter(|(name, path)| (all || names.contains(*name)) && !path.is_empty())
                .map(|(_, path)| path.clone())
                .collect()
        }
    };

    // r[impl sched.merge.stale-substitutable+3]
    // Realized-path carrier (migration 082, the floating-CA
    // stale-reset lane): the empty-path slots filtered above are the
    // floating-CA placeholders — for a carried job the realized paths
    // ride the job row, written at creation by the stale_reset origin
    // (an immutable content-addressed snapshot; the wanted NAME set
    // above stays live). The scheduler-side consumption coverage reads
    // the same column, so seed set and coverage agree by construction.
    // bug_027: the union is UNCONDITIONAL — hoisted ABOVE every return
    // path (the pre-fix zero-live-rows early return skipped it, so a
    // carried job whose interest vanished mid-claim reported
    // InfraFailure here while the scheduler's consumption leg, which
    // unions the carrier BEFORE its LiveWanted width check, computed
    // non-zero width and charged the park budget for the race
    // merged_bug_194 closed uncharged). bug_233's
    // parse-don't-validate keeps the read itself total: every
    // ClaimedJob carries a real job_id.
    let carried: Option<Vec<String>> = sqlx::query_scalar(
        "SELECT carried_realized_paths FROM materialization_jobs \
          WHERE job_id = $1",
    )
    .bind(claimed.job_id)
    .fetch_optional(&ctx.pool)
    .await?
    .flatten();
    for p in carried.unwrap_or_default() {
        if !p.is_empty() && !paths.contains(&p) {
            paths.push(p);
        }
    }

    Ok(paths)
}

/// The store-side pin-at-ingest INSERT (design §5.1).
///
/// Executes `rio_migrations::sql::PIN_MATERIALIZED_UPSERT_SQL` — the
/// ONE shared text `SchedulerDb::pin_materialized_paths`
/// (rio-scheduler/src/db/live_pins.rs) also runs (bug_192; PD-13:
/// rio-store cannot link rio-scheduler, both link rio-migrations).
/// Binds 1-element arrays. Under the 093 key the materialization pin
/// is its OWN row: a build_input pin for the same (path, drv) is never
/// re-kinded (bug_253), and re-pinning refreshes job_id idempotently.
// r[impl sched.materialize.pinning]
// r[impl obs.metric.store]
async fn pin_materialized_path(
    ctx: &ExecutorContext,
    claimed: &ClaimedJob,
    store_path: &str,
) -> Result<(), sqlx::Error> {
    use sha2::Digest;
    let path_hash = sha2::Sha256::digest(store_path.as_bytes()).to_vec();
    sqlx::query(rio_migrations::sql::PIN_MATERIALIZED_UPSERT_SQL)
        .bind(std::slice::from_ref(&path_hash))
        .bind(std::slice::from_ref(&claimed.drv_hash))
        .bind(claimed.job_id)
        .execute(&ctx.pool)
        .await?;
    // T-6.2: the pin-supply counter — pairs with the scheduler's §5.3
    // release lifecycle for pin-leak detection (a pinned-paths rate with
    // no matching release activity after jobs resolve means pins are
    // accumulating).
    metrics::counter!("rio_store_materialization_pinned_paths_total").increment(1);
    Ok(())
}

/// One tenant's HEAD-probe answer for a clean substitute miss (B3's
/// executor half, per-tenant since merged_bug_028). A confirmed-absent
/// VERDICT additionally requires confirmation under EVERY interested
/// tenant plus the caller's [`LocalMiss`] witness (bug_042) — this
/// per-tenant probe alone never constructs one.
///
/// bug_295: a failed probe carries its CLASS, and the caller routes
/// the disposition through `classify_substitute_failure` — the same
/// truth table the substitution attempt leg uses. The HEAD and GET
/// legs are congruent per class: a 429'd probe closes UNCHARGED
/// (RateLimited → RetryUncharged; pre-fix it was laundered into
/// `indeterminate` and charged, so a rate-limit wave on the PROBE
/// burned the park budget the attempt leg was already protecting),
/// while 5xx/timeout/transport probes stay CHARGED (Fetch →
/// ChargeInfra — a GET 5xx charges, so a HEAD 5xx must too). The
/// per-call deadline cut also charges: it is our own budget
/// infrastructure failing to classify, not upstream politeness
/// (rationale recorded in the substitution-replacement invariant
/// map).
/// bug_194: THE class→(label, message) chokepoint shared by the
/// attempt and probe legs. Transient classes (rate-limited / raced)
/// get NEUTRAL text plus their label — their deferral is uncharged
/// and the scheduler logs the detail verbatim, so "infrastructure
/// trouble" wording on a 429 narrated a contradiction against
/// class="rate_limited". Charging classes keep the infrastructure
/// framing (the charge ladder sees them). One derivation site: a leg
/// cannot drift its wording from the class again — the attempt leg's
/// shape (formerly inline at the tenant loop) is the source.
fn substitute_cell_message(
    path: &str,
    class: rio_evidence_kernel::outcome::SubstituteFailureClass,
    detail: impl core::fmt::Display,
) -> (&'static str, String) {
    use rio_evidence_kernel::outcome::SubstituteFailureClass as C;
    match class {
        C::RateLimited => ("rate_limited", format!("substitution of {path}: {detail}")),
        C::Raced => ("raced", format!("substitution of {path}: {detail}")),
        _ => (
            "",
            format!("substitution of {path} hit infrastructure trouble ({class:?}): {detail}"),
        ),
    }
}

enum MissProbe {
    /// This tenant's upstreams definitively answered "not present".
    Confirmed,
    /// The probe failed with a substitute-failure class; the caller
    /// folds the classified disposition with every other tenant's.
    Failed {
        /// The probe-leg failure class (RateLimited for a terminal
        /// 429; Fetch for 5xx/timeout/deadline/present-but-not-
        /// ingested; the error path maps through
        /// `substitute_error_evidence`).
        class: SubstituteFailureClass,
        /// Parsed `Retry-After` advice (RateLimited only).
        retry_after: Option<std::time::Duration>,
        /// Human detail for the outcome message.
        detail: String,
    },
}

/// Distinguish confirmed-absent from infrastructure trouble after a
/// `try_substitute` miss, using the HEAD-probe machinery
/// ([`Substituter::check_available`]): a path in `rate_limited` is a
/// terminal 429 (transient class); a path in `indeterminate` could
/// not be classified (charging class); a path in `hits` is present
/// upstream but substitution did not ingest it (also charging —
/// something is wrong on our side); a path in none of the sets is a
/// confirmed miss UNDER THIS TENANT.
// r[impl store.materialize.probe-polarity]
async fn probe_miss(ctx: &ExecutorContext, tenant_id: Uuid, store_path: &str) -> MissProbe {
    let deadline = tokio::time::Instant::now() + MISS_PROBE_DEADLINE;
    let paths = [store_path.to_string()];
    match ctx
        .substituter
        .check_available(tenant_id, &paths, deadline)
        .await
    {
        Ok(result) => {
            if let Some((_, retry_after)) =
                result.rate_limited.iter().find(|(p, _)| p == store_path)
            {
                MissProbe::Failed {
                    class: SubstituteFailureClass::RateLimited,
                    retry_after: *retry_after,
                    detail: "availability probe rate-limited (upstream 429)".to_string(),
                }
            } else if result.indeterminate.iter().any(|p| p == store_path) {
                MissProbe::Failed {
                    class: SubstituteFailureClass::Fetch,
                    retry_after: None,
                    detail: "availability probe indeterminate (upstream 5xx/timeout)".to_string(),
                }
            } else if result.hits.iter().any(|p| p == store_path) {
                MissProbe::Failed {
                    class: SubstituteFailureClass::Fetch,
                    retry_after: None,
                    detail: "upstream reports the path present but substitution did not ingest it"
                        .to_string(),
                }
            } else {
                MissProbe::Confirmed
            }
        }
        Err(e) => {
            let (class, retry_after) = crate::substitute::substitute_error_evidence(&e);
            MissProbe::Failed {
                class,
                retry_after,
                detail: format!("availability probe failed: {e}"),
            }
        }
    }
}

// ---------------------------------------------------------------------------
// PG + fake-upstream battery
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::metadata::{self, SigMode};
    use crate::signing::Signer;
    use crate::test_helpers::seed_tenant;
    use rio_nix::narinfo::fingerprint;
    use rio_nix::store_path::StorePath;
    use rio_test_support::TestDb;
    use std::net::SocketAddr;

    // ── Fixtures ──────────────────────────────────────────────────────

    /// A store path with a DISTINCT hash part per tag (the substitute.rs
    /// fixture uses one fixed hash for every path, which collides when a
    /// fake upstream serves several paths — narinfo URLs key on the hash
    /// part).
    fn store_path(tag: u8, name: &str) -> String {
        // The REAL nixbase32 alphabet (no e/o/u/t) — an invalid char
        // fails StorePath::parse with InvalidBase32Char.
        const ALPHABET: &[u8] = b"0123456789abcdfghijklmnpqrsvwxyz";
        let c = char::from(ALPHABET[(tag as usize) % ALPHABET.len()]);
        let hash: String = std::iter::repeat_n(c, 32).collect();
        format!("/nix/store/{hash}-{name}")
    }

    /// In-process multi-path upstream: serves narinfo + NAR for each
    /// `(store_path, nar_bytes, full_path_references)` triple, all
    /// signed by one key. References appear as basenames in the
    /// narinfo text and as full paths in the signed fingerprint (the
    /// Nix convention).
    struct FakeUpstream {
        url: String,
        trusted_key: String,
        _task: tokio::task::JoinHandle<()>,
    }

    async fn spawn_multi_upstream(
        paths: Vec<(String, Vec<u8>, Vec<String>)>,
        key_name: &str,
    ) -> FakeUpstream {
        use axum::{Router, routing::get};
        use base64::Engine;
        use sha2::Digest;

        let seed = [0x42u8; 32];
        let signer = Signer::from_seed(key_name, &seed);
        let pubkey = ed25519_dalek::SigningKey::from_bytes(&seed).verifying_key();
        let trusted_key = format!(
            "{key_name}:{}",
            base64::engine::general_purpose::STANDARD.encode(pubkey.as_bytes())
        );

        let mut app = Router::new().route(
            "/nix-cache-info",
            get(|| async { "StoreDir: /nix/store\nWantMassQuery: 1\nPriority: 40\n" }),
        );
        for (path, nar, refs) in paths {
            let nar_hash: [u8; 32] = sha2::Sha256::digest(&nar).into();
            let nar_hash_str = format!(
                "sha256:{}",
                rio_nix::store_path::nixbase32::encode(&nar_hash)
            );
            let fp = fingerprint(&path, &nar_hash, nar.len() as u64, &refs);
            let sig = signer.sign(&fp);
            let sp = StorePath::parse(&path).unwrap();
            let hash_part = sp.hash_part();
            // narinfo references are basenames (store dir implicit).
            let ref_basenames: Vec<&str> = refs
                .iter()
                .map(|r| r.strip_prefix("/nix/store/").unwrap_or(r))
                .collect();
            let narinfo = format!(
                "StorePath: {path}\n\
                 URL: nar/{hash_part}.nar\n\
                 Compression: none\n\
                 NarHash: {nar_hash_str}\n\
                 NarSize: {}\n\
                 References: {}\n\
                 Sig: {sig}\n",
                nar.len(),
                ref_basenames.join(" ")
            );
            app = app
                .route(
                    &format!("/{hash_part}.narinfo"),
                    get(move || async move { narinfo }),
                )
                .route(
                    &format!("/nar/{hash_part}.nar"),
                    get(move || async move { nar }),
                );
        }

        let listener = tokio::net::TcpListener::bind(SocketAddr::from(([127, 0, 0, 1], 0)))
            .await
            .unwrap();
        let addr = listener.local_addr().unwrap();
        let task = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        FakeUpstream {
            url: format!("http://{addr}"),
            trusted_key,
            _task: task,
        }
    }

    /// An upstream that answers every request with the given status
    /// (the substitute.rs `spawn_status_upstream` precedent).
    async fn spawn_status_upstream(status: axum::http::StatusCode) -> FakeUpstream {
        use axum::{Router, routing::any};
        let app = Router::new().fallback(any(move || async move { status }));
        let listener = tokio::net::TcpListener::bind(SocketAddr::from(([127, 0, 0, 1], 0)))
            .await
            .unwrap();
        let addr = listener.local_addr().unwrap();
        let task = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        FakeUpstream {
            url: format!("http://{addr}"),
            trusted_key: "unused:AAAA".to_string(),
            _task: task,
        }
    }

    /// bug_041: a multi-path upstream whose NAR route GATES — the first
    /// NAR request signals `hit_tx` and then waits for `release` —
    /// so a test can deterministically mutate live interest WHILE the
    /// walk is mid-fetch (the seam between iteration 1's ingest and
    /// iteration 2's wanted re-read).
    async fn spawn_gated_upstream(
        paths: Vec<(String, Vec<u8>, Vec<String>)>,
        key_name: &str,
    ) -> (
        FakeUpstream,
        tokio::sync::mpsc::UnboundedReceiver<()>,
        std::sync::Arc<tokio::sync::Notify>,
    ) {
        use axum::{Router, routing::get};
        use base64::Engine;
        use sha2::Digest;

        let seed = [0x43u8; 32];
        let signer = Signer::from_seed(key_name, &seed);
        let pubkey = ed25519_dalek::SigningKey::from_bytes(&seed).verifying_key();
        let trusted_key = format!(
            "{key_name}:{}",
            base64::engine::general_purpose::STANDARD.encode(pubkey.as_bytes())
        );
        let (hit_tx, hit_rx) = tokio::sync::mpsc::unbounded_channel::<()>();
        let release = std::sync::Arc::new(tokio::sync::Notify::new());

        let mut app = Router::new().route(
            "/nix-cache-info",
            get(|| async { "StoreDir: /nix/store\nWantMassQuery: 1\nPriority: 40\n" }),
        );
        for (path, nar, refs) in paths {
            let nar_hash: [u8; 32] = sha2::Sha256::digest(&nar).into();
            let nar_hash_str = format!(
                "sha256:{}",
                rio_nix::store_path::nixbase32::encode(&nar_hash)
            );
            let fp = fingerprint(&path, &nar_hash, nar.len() as u64, &refs);
            let sig = signer.sign(&fp);
            let sp = StorePath::parse(&path).unwrap();
            let hash_part = sp.hash_part();
            let ref_basenames: Vec<&str> = refs
                .iter()
                .map(|r| r.strip_prefix("/nix/store/").unwrap_or(r))
                .collect();
            let narinfo = format!(
                "StorePath: {path}\n\
                 URL: nar/{hash_part}.nar\n\
                 Compression: none\n\
                 NarHash: {nar_hash_str}\n\
                 NarSize: {}\n\
                 References: {}\n\
                 Sig: {sig}\n",
                nar.len(),
                ref_basenames.join(" ")
            );
            let tx = hit_tx.clone();
            let rel = std::sync::Arc::clone(&release);
            app = app
                .route(
                    &format!("/{hash_part}.narinfo"),
                    get(move || async move { narinfo }),
                )
                .route(
                    &format!("/nar/{hash_part}.nar"),
                    get(move || async move {
                        let _ = tx.send(());
                        rel.notified().await;
                        nar
                    }),
                );
        }

        let listener = tokio::net::TcpListener::bind(SocketAddr::from(([127, 0, 0, 1], 0)))
            .await
            .unwrap();
        let addr = listener.local_addr().unwrap();
        let task = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        (
            FakeUpstream {
                url: format!("http://{addr}"),
                trusted_key,
                _task: task,
            },
            hit_rx,
            release,
        )
    }

    /// merged_bug_016: a multi-path upstream with a kill switch — once
    /// `gone` is set, EVERY route answers 404 (the upstream evicted
    /// the path between the dispatch-time HEAD probe and the
    /// execution-time GET).
    async fn spawn_flippable_upstream(
        paths: Vec<(String, Vec<u8>, Vec<String>)>,
        key_name: &str,
    ) -> (FakeUpstream, std::sync::Arc<std::sync::atomic::AtomicBool>) {
        use axum::{Router, http::StatusCode, routing::get};
        use base64::Engine;
        use sha2::Digest;
        use std::sync::Arc;
        use std::sync::atomic::{AtomicBool, Ordering};

        let gone = Arc::new(AtomicBool::new(false));
        let seed = [0x42u8; 32];
        let signer = Signer::from_seed(key_name, &seed);
        let pubkey = ed25519_dalek::SigningKey::from_bytes(&seed).verifying_key();
        let trusted_key = format!(
            "{key_name}:{}",
            base64::engine::general_purpose::STANDARD.encode(pubkey.as_bytes())
        );

        let mut app = Router::new().route(
            "/nix-cache-info",
            get(|| async { "StoreDir: /nix/store\nWantMassQuery: 1\nPriority: 40\n" }),
        );
        for (path, nar, refs) in paths {
            let nar_hash: [u8; 32] = sha2::Sha256::digest(&nar).into();
            let nar_hash_str = format!(
                "sha256:{}",
                rio_nix::store_path::nixbase32::encode(&nar_hash)
            );
            let fp = fingerprint(&path, &nar_hash, nar.len() as u64, &refs);
            let sig = signer.sign(&fp);
            let sp = StorePath::parse(&path).unwrap();
            let hash_part = sp.hash_part();
            let ref_basenames: Vec<&str> = refs
                .iter()
                .map(|r| r.strip_prefix("/nix/store/").unwrap_or(r))
                .collect();
            let narinfo = format!(
                "StorePath: {path}\n\
                 URL: nar/{hash_part}.nar\n\
                 Compression: none\n\
                 NarHash: {nar_hash_str}\n\
                 NarSize: {}\n\
                 References: {}\n\
                 Sig: {sig}\n",
                nar.len(),
                ref_basenames.join(" ")
            );
            let g1 = gone.clone();
            let g2 = gone.clone();
            app = app
                .route(
                    &format!("/{hash_part}.narinfo"),
                    get(move || async move {
                        if g1.load(Ordering::SeqCst) {
                            Err(StatusCode::NOT_FOUND)
                        } else {
                            Ok(narinfo)
                        }
                    }),
                )
                .route(
                    &format!("/nar/{hash_part}.nar"),
                    get(move || async move {
                        if g2.load(Ordering::SeqCst) {
                            Err(StatusCode::NOT_FOUND)
                        } else {
                            Ok(nar)
                        }
                    }),
                );
        }

        let listener = tokio::net::TcpListener::bind(SocketAddr::from(([127, 0, 0, 1], 0)))
            .await
            .unwrap();
        let addr = listener.local_addr().unwrap();
        let task = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        (
            FakeUpstream {
                url: format!("http://{addr}"),
                trusted_key,
                _task: task,
            },
            gone,
        )
    }

    /// Sandbox-safe reqwest client (the substitute.rs precedent: no CA
    /// bundle in the nix sandbox; the fake upstream is plaintext).
    use crate::test_helpers::sandbox_http;

    /// The battery context runs at the PRODUCTION fan-out (F = 4, the
    /// WO-R7-3 default — gate parity per §6.2: a width-1 battery would
    /// exercise a regime production does not use, hiding interleaving
    /// bugs from the gate). Width-sensitive tests override the field.
    fn make_ctx(pool: PgPool) -> ExecutorContext {
        ExecutorContext::new(
            pool.clone(),
            std::sync::Arc::new(Substituter::new(pool, None).with_http_client(sandbox_http())),
            4,
            PathSlotPool::new(32),
        )
    }

    /// Mint a [`ClaimAdmission`] through the PRODUCTION gate (R13 — no
    /// bypass constructor): tests model an admitted claim exactly the
    /// way `claim_loop` creates one (slot ≺ claim).
    fn admitted(ctx: &ExecutorContext) -> ClaimAdmission {
        ctx.path_slots
            .try_admit_claim()
            .expect("test pool has headroom for the claim admission")
    }

    /// Seed the scheduler-side rows one claimed job needs: a
    /// derivation (with output↔path arrays), a live build wanting
    /// `wanted_names` of it, a pending materialization job, and the
    /// wanted relation. Returns the ClaimedJob the executor would have
    /// been handed after a successful claim.
    struct SeededJob {
        claimed: ClaimedJob,
        build_id: Uuid,
        derivation_id: Uuid,
        job_id: Uuid,
    }

    async fn seed_job(
        pool: &PgPool,
        drv_hash: &str,
        outputs: &[(&str, &str)], // (output_name, expected_path)
        build_tenant: Option<Uuid>,
        job_tenant: Option<Uuid>,
        wanted_names: &[&str],
    ) -> SeededJob {
        let names: Vec<String> = outputs.iter().map(|(n, _)| n.to_string()).collect();
        let paths: Vec<String> = outputs.iter().map(|(_, p)| p.to_string()).collect();
        let derivation_id: Uuid = sqlx::query_scalar(
            "INSERT INTO derivations \
                 (drv_hash, drv_path, system, status, output_names, expected_output_paths) \
             VALUES ($1, $2, 'x86_64-linux', 'ready', $3, $4) \
             RETURNING derivation_id",
        )
        .bind(drv_hash)
        .bind(format!("/nix/store/{drv_hash}.drv"))
        .bind(&names)
        .bind(&paths)
        .fetch_one(pool)
        .await
        .expect("derivation seeded");

        let build_id = Uuid::new_v4();
        sqlx::query("INSERT INTO builds (build_id, tenant_id, status) VALUES ($1, $2, 'active')")
            .bind(build_id)
            .bind(build_tenant)
            .execute(pool)
            .await
            .expect("build seeded");

        // 086: interest derives from membership; production records it
        // at merge (db/batch.rs).
        sqlx::query("INSERT INTO build_derivations (build_id, derivation_id) VALUES ($1, $2)")
            .bind(build_id)
            .bind(derivation_id)
            .execute(pool)
            .await
            .expect("membership seeded");
        let wanted: Vec<String> = wanted_names.iter().map(|s| s.to_string()).collect();
        sqlx::query(
            "INSERT INTO build_wanted_outputs (build_id, derivation_id, wanted_output_names) \
             VALUES ($1, $2, $3)",
        )
        .bind(build_id)
        .bind(derivation_id)
        .bind(&wanted)
        .execute(pool)
        .await
        .expect("wanted relation seeded");

        let job_id = Uuid::now_v7();
        sqlx::query(
            "INSERT INTO materialization_jobs \
                 (job_id, derivation_id, drv_hash, tenant_id, origin, created_generation) \
             VALUES ($1, $2, $3, $4, 'cache_opportunity', 1)",
        )
        .bind(job_id)
        .bind(derivation_id)
        .bind(drv_hash)
        .bind(job_tenant)
        .execute(pool)
        .await
        .map(|_| ())
        .unwrap_or_else(|e| panic!("job seeded: {e}"));

        SeededJob {
            claimed: ClaimedJob {
                job_id,
                drv_hash: drv_hash.to_string(),
                tenant_hint: job_tenant,
                origin: "cache_opportunity".to_string(),
                exec_id: Uuid::now_v7().to_string(),
                drv_path: format!("/nix/store/{drv_hash}.drv"),
            },
            build_id,
            derivation_id,
            job_id,
        }
    }

    /// Configure `upstream` for `tenant`.
    async fn wire_upstream(pool: &PgPool, tenant: Uuid, upstream: &FakeUpstream) {
        metadata::upstreams::insert(
            pool,
            tenant,
            &upstream.url,
            50,
            std::slice::from_ref(&upstream.trusted_key),
            SigMode::Keep,
        )
        .await
        .expect("upstream wired");
    }

    /// Pin count for a drv by kind (the assertion the pinning tests use).
    async fn pin_count(pool: &PgPool, drv: &str, kind: &str) -> i64 {
        sqlx::query_scalar(
            "SELECT COUNT(*) FROM scheduler_live_pins WHERE drv_hash = $1 AND pin_kind = $2",
        )
        .bind(drv)
        .bind(kind)
        .fetch_one(pool)
        .await
        .expect("pin count")
    }

    fn outcome_success(o: &MaterializationOutcome) -> Option<&materialization_outcome::Success> {
        match o.outcome.as_ref()? {
            materialization_outcome::Outcome::Success(s) => Some(s),
            _ => None,
        }
    }

    fn outcome_unobtainable(
        o: &MaterializationOutcome,
    ) -> Option<&materialization_outcome::Unobtainable> {
        match o.outcome.as_ref()? {
            materialization_outcome::Outcome::Unobtainable(u) => Some(u),
            _ => None,
        }
    }

    fn outcome_infra(o: &MaterializationOutcome) -> Option<&materialization_outcome::InfraFailure> {
        match o.outcome.as_ref()? {
            materialization_outcome::Outcome::InfraFailure(f) => Some(f),
            _ => None,
        }
    }

    // ── The battery ───────────────────────────────────────────────────

    // r[verify store.materialize.executor+5]
    // r[verify sched.materialize.pinning]
    /// (1) The walk: BFS over narinfo references from the wanted seed
    /// path; every closure member try_substitute'd in-process; every
    /// ingested path pinned (pin_kind='materialization', job_id
    /// stamped) BEFORE the Success report exists. Closure-complete or
    /// no Success: the report covers root + transitive dep.
    #[tokio::test]
    async fn execution_walks_references_pins_and_reports_success() {
        let db = TestDb::new(&crate::MIGRATOR).await;
        let tenant = seed_tenant(&db.pool, "mat-walk").await;

        // A two-node closure: root references dep.
        let root = store_path(1, "mat-root");
        let dep = store_path(2, "mat-dep");
        let (root_nar, _) = rio_test_support::fixtures::make_nar(b"root contents");
        let (dep_nar, _) = rio_test_support::fixtures::make_nar(b"dep contents");
        let upstream = spawn_multi_upstream(
            vec![
                (root.clone(), root_nar, vec![dep.clone()]),
                (dep.clone(), dep_nar, vec![]),
            ],
            "cache.walk",
        )
        .await;
        wire_upstream(&db.pool, tenant, &upstream).await;

        let seeded = seed_job(
            &db.pool,
            "mat-walk-drv",
            &[("out", root.as_str())],
            Some(tenant),
            Some(tenant),
            &[], // '{}' = all outputs wanted
        )
        .await;

        let ctx = make_ctx(db.pool.clone());
        let outcome = execute_job(&ctx, &seeded.claimed, admitted(&ctx))
            .await
            .into_outcome();

        let success = outcome_success(&outcome)
            .unwrap_or_else(|| panic!("expected Success, got {outcome:?}"));
        let mut covered: Vec<&str> = success
            .ingested_paths
            .iter()
            .chain(success.verified_paths.iter())
            .map(String::as_str)
            .collect();
        covered.sort_unstable();
        let mut expected = vec![root.as_str(), dep.as_str()];
        expected.sort_unstable();
        assert_eq!(
            covered, expected,
            "the closure walk covers the wanted root AND its narinfo reference"
        );

        // Pin-at-ingest: BOTH closure members are pinned with the
        // materialization kind and the job id.
        assert_eq!(
            pin_count(&db.pool, "mat-walk-drv", "materialization").await,
            2,
            "every ingested/verified path is pinned at ingest"
        );
        let pinned_jobs: Vec<Option<Uuid>> =
            sqlx::query_scalar("SELECT job_id FROM scheduler_live_pins WHERE drv_hash = $1")
                .bind("mat-walk-drv")
                .fetch_all(&db.pool)
                .await
                .unwrap();
        assert!(
            pinned_jobs.iter().all(|j| *j == Some(seeded.job_id)),
            "pins carry the resolving job's id"
        );

        // The paths actually landed in the store (narinfo rows exist).
        for p in [&root, &dep] {
            assert!(
                crate::metadata::query_path_info(&db.pool, p)
                    .await
                    .unwrap()
                    .is_some(),
                "{p} was ingested into the store"
            );
        }
    }

    // r[verify store.materialize.executor+5]
    /// (2) The wanted set is read at execution time and RE-READ at the
    // r[verify sched.merge.stale-substitutable+3]
    /// Floating-CA stale-reset carrier (migration 082): a job whose
    /// derivation row has the empty-path placeholder (`("out", "")`)
    /// resolves its wanted set through the job row's
    /// `carried_realized_paths` — the executor fetches the realized
    /// path instead of producing the vacuous `Success{[],[]}` the
    /// empty-filtered map yields pre-fix (zero fetches, zero pins; the
    /// scheduler then "re-completed" the node with `[""]`).
    #[tokio::test]
    async fn floating_ca_carried_paths_seed_the_walk() {
        let db = TestDb::new(&crate::MIGRATOR).await;
        let tenant = seed_tenant(&db.pool, "mat-fca").await;

        let real = store_path(7, "mat-fca-realized");
        let (nar, _) = rio_test_support::fixtures::make_nar(b"fca contents");
        let upstream = spawn_multi_upstream(vec![(real.clone(), nar, vec![])], "cache.fca").await;
        wire_upstream(&db.pool, tenant, &upstream).await;

        // Floating-CA shape: the expected slot is the "" placeholder.
        let seeded = seed_job(
            &db.pool,
            "mat-fca-drv",
            &[("out", "")],
            Some(tenant),
            Some(tenant),
            &[], // '{}' = all outputs wanted
        )
        .await;
        // The stale_reset origin wrote the carrier at creation.
        sqlx::query(
            "UPDATE materialization_jobs SET origin = 'stale_reset', carried_realized_paths = $2 WHERE job_id = $1",
        )
        .bind(seeded.job_id)
        .bind(vec![real.clone()])
        .execute(&db.pool)
        .await
        .expect("carrier seeded");

        let ctx = make_ctx(db.pool.clone());
        let outcome = execute_job(&ctx, &seeded.claimed, admitted(&ctx))
            .await
            .into_outcome();

        let success = outcome_success(&outcome)
            .unwrap_or_else(|| panic!("expected Success, got {outcome:?}"));
        let covered: Vec<&str> = success
            .ingested_paths
            .iter()
            .chain(success.verified_paths.iter())
            .map(String::as_str)
            .collect();
        assert!(
            covered.contains(&real.as_str()),
            "the carried realized path must seed the walk (pre-fix: \
             vacuous Success {{ingested: [], verified: []}}, zero \
             fetches); covered={covered:?}"
        );
        // Pin-at-ingest applies to the carried path like any other.
        assert_eq!(
            pin_count(&db.pool, "mat-fca-drv", "materialization").await,
            1,
            "the carried fetch pins at ingest"
        );
    }

    /// final verification pass — never snapshotted at creation. The
    /// job is created while only `out` is wanted; by execution time a
    /// second live build wants `dev` too; the executor fetches BOTH.
    /// Then: mid-execution growth — a third build adds `doc` while the
    /// walk runs; the final verification pass picks it up.
    #[tokio::test]
    async fn execution_rereads_wanted_at_final_verification() {
        let db = TestDb::new(&crate::MIGRATOR).await;
        let tenant = seed_tenant(&db.pool, "mat-reread").await;

        let out_path = store_path(3, "mat-out");
        let dev_path = store_path(4, "mat-dev");
        let (out_nar, _) = rio_test_support::fixtures::make_nar(b"out");
        let (dev_nar, _) = rio_test_support::fixtures::make_nar(b"dev");
        let upstream = spawn_multi_upstream(
            vec![
                (out_path.clone(), out_nar, vec![]),
                (dev_path.clone(), dev_nar, vec![]),
            ],
            "cache.reread",
        )
        .await;
        wire_upstream(&db.pool, tenant, &upstream).await;

        // Created wanting only "out"...
        let seeded = seed_job(
            &db.pool,
            "mat-reread-drv",
            &[("out", out_path.as_str()), ("dev", dev_path.as_str())],
            Some(tenant),
            Some(tenant),
            &["out"],
        )
        .await;

        // ...but by execution time, a SECOND live build wants "dev".
        // (The creation-time snapshot would miss it; the execution-time
        // read must not.)
        let build2 = Uuid::new_v4();
        sqlx::query("INSERT INTO builds (build_id, tenant_id, status) VALUES ($1, $2, 'active')")
            .bind(build2)
            .bind(tenant)
            .execute(&db.pool)
            .await
            .unwrap();
        sqlx::query(
            "INSERT INTO build_wanted_outputs (build_id, derivation_id, wanted_output_names) \
             VALUES ($1, $2, $3)",
        )
        .bind(build2)
        .bind(seeded.derivation_id)
        .bind(vec!["dev".to_string()])
        .execute(&db.pool)
        .await
        .unwrap();
        sqlx::query("INSERT INTO build_derivations (build_id, derivation_id) VALUES ($1, $2)")
            .bind(build2)
            .bind(seeded.derivation_id)
            .execute(&db.pool)
            .await
            .unwrap();

        let ctx = make_ctx(db.pool.clone());
        let outcome = execute_job(&ctx, &seeded.claimed, admitted(&ctx))
            .await
            .into_outcome();

        let success = outcome_success(&outcome)
            .unwrap_or_else(|| panic!("expected Success, got {outcome:?}"));
        let covered: HashSet<&str> = success
            .ingested_paths
            .iter()
            .chain(success.verified_paths.iter())
            .map(String::as_str)
            .collect();
        assert!(
            covered.contains(out_path.as_str()) && covered.contains(dev_path.as_str()),
            "execution-time wanted (out + dev) is covered, not the creation-time \
             snapshot (out only): covered = {covered:?}"
        );
    }

    // r[verify store.materialize.executor+5]
    /// (3) Tenant resolution (AS-4): job tenant NULL + no live
    /// interested build with a tenant → InfraFailure{no-tenant-context}.
    /// Never Unobtainable, never silent success.
    #[tokio::test]
    async fn no_tenant_context_reports_infra_failure() {
        let db = TestDb::new(&crate::MIGRATOR).await;
        let path = store_path(5, "mat-no-tenant");

        // Job tenant NULL; the live interested build also has NULL
        // tenant (single-tenant/dev shape).
        let seeded = seed_job(
            &db.pool,
            "mat-no-tenant-drv",
            &[("out", path.as_str())],
            None,
            None,
            &[],
        )
        .await;

        let ctx = make_ctx(db.pool.clone());
        let outcome = execute_job(&ctx, &seeded.claimed, admitted(&ctx))
            .await
            .into_outcome();

        let infra = outcome_infra(&outcome)
            .unwrap_or_else(|| panic!("expected InfraFailure, got {outcome:?}"));
        assert!(
            infra.detail.contains("no-tenant-context"),
            "the detail names the AS-4 condition: {}",
            infra.detail
        );
        assert_eq!(
            pin_count(&db.pool, "mat-no-tenant-drv", "materialization").await,
            0,
            "nothing is pinned when nothing is fetched"
        );
    }

    // r[verify store.materialize.executor+5]
    /// (4) Stale recorded tenant (PDQ-8): the creating build is
    /// TERMINAL (its recorded tenant no longer carries live interest)
    /// while a live interested build with a DIFFERENT tenant remains →
    /// the executor fetches under the live build's tenant. Proven by
    /// upstream wiring: only the live tenant has the upstream
    /// configured, so success is only possible under re-resolution.
    #[tokio::test]
    async fn execution_stale_recorded_tenant_uses_live_interest() {
        let db = TestDb::new(&crate::MIGRATOR).await;
        let stale_tenant = seed_tenant(&db.pool, "mat-stale").await;
        let live_tenant = seed_tenant(&db.pool, "mat-live").await;

        let path = store_path(6, "mat-stale-tenant");
        let (nar, _) = rio_test_support::fixtures::make_nar(b"stale tenant contents");
        let upstream = spawn_multi_upstream(vec![(path.clone(), nar, vec![])], "cache.stale").await;
        // ONLY the live tenant can reach the upstream.
        wire_upstream(&db.pool, live_tenant, &upstream).await;

        // The creating build (stale tenant, recorded on the job) is
        // TERMINAL; a different live build carries live_tenant.
        let seeded = seed_job(
            &db.pool,
            "mat-stale-drv",
            &[("out", path.as_str())],
            Some(stale_tenant),
            Some(stale_tenant),
            &[],
        )
        .await;
        sqlx::query("UPDATE builds SET status = 'succeeded' WHERE build_id = $1")
            .bind(seeded.build_id)
            .execute(&db.pool)
            .await
            .unwrap();
        let live_build = Uuid::new_v4();
        sqlx::query("INSERT INTO builds (build_id, tenant_id, status) VALUES ($1, $2, 'active')")
            .bind(live_build)
            .bind(live_tenant)
            .execute(&db.pool)
            .await
            .unwrap();
        sqlx::query(
            "INSERT INTO build_wanted_outputs (build_id, derivation_id, wanted_output_names) \
             VALUES ($1, $2, '{}')",
        )
        .bind(live_build)
        .bind(seeded.derivation_id)
        .execute(&db.pool)
        .await
        .unwrap();
        sqlx::query("INSERT INTO build_derivations (build_id, derivation_id) VALUES ($1, $2)")
            .bind(live_build)
            .bind(seeded.derivation_id)
            .execute(&db.pool)
            .await
            .unwrap();

        let ctx = make_ctx(db.pool.clone());
        let outcome = execute_job(&ctx, &seeded.claimed, admitted(&ctx))
            .await
            .into_outcome();

        let success = outcome_success(&outcome).unwrap_or_else(|| {
            panic!(
                "expected Success under the LIVE tenant's upstream (re-resolution), \
                 got {outcome:?} — the executor used the stale recorded tenant"
            )
        });
        assert_eq!(
            success.ingested_paths.len() + success.verified_paths.len(),
            1,
            "the wanted path was fetched under the live tenant"
        );
    }

    // r[verify store.materialize.executor+5]
    /// bug_041 red: the walk re-reads live wanted EVERY iteration but
    /// pre-fix snapshotted the tenant set ONCE — seeds arriving from a
    /// mid-walk interest shift were probed under the DEPARTED tenant
    /// set, and with the old tenant's upstreams unable to serve them
    /// the path compiled to Unobtainable without the actually-live
    /// tenant's upstreams ever being consulted (violating owner-Q2:
    /// "the job fails only when NO interested tenant can obtain").
    ///
    /// Choreography: tenant A's gated upstream serves P and BLOCKS the
    /// NAR fetch; while blocked, A's build goes terminal and tenant
    /// B's build arrives wanting {P, Q}; the gate releases; iteration
    /// 2's wanted re-read finds the new seed Q — which only B's
    /// upstream serves.
    #[tokio::test]
    async fn mid_walk_interest_shift_probes_under_live_tenants() {
        let db = TestDb::new(&crate::MIGRATOR).await;
        let tenant_a = seed_tenant(&db.pool, "mat-shift-a").await;
        let tenant_b = seed_tenant(&db.pool, "mat-shift-b").await;

        let path_p = store_path(11, "mat-shift-p");
        let path_q = store_path(12, "mat-shift-q");
        let (nar_p, _) = rio_test_support::fixtures::make_nar(b"shift contents p");
        let (nar_q, _) = rio_test_support::fixtures::make_nar(b"shift contents q");

        // Tenant A: gated upstream serving ONLY P.
        let (up_a, mut hit_rx, release) =
            spawn_gated_upstream(vec![(path_p.clone(), nar_p, vec![])], "cache.shift-a").await;
        wire_upstream(&db.pool, tenant_a, &up_a).await;
        // Tenant B: serves ONLY Q (the post-shift seed).
        let up_b =
            spawn_multi_upstream(vec![(path_q.clone(), nar_q, vec![])], "cache.shift-b").await;
        wire_upstream(&db.pool, tenant_b, &up_b).await;

        // Build A wants only "out" (P). The drv declares out=P, doc=Q.
        let seeded = seed_job(
            &db.pool,
            "mat-shift-drv",
            &[("out", path_p.as_str()), ("doc", path_q.as_str())],
            Some(tenant_a),
            Some(tenant_a),
            &["out"],
        )
        .await;

        let ctx = make_ctx(db.pool.clone());
        let claimed = seeded.claimed.clone();
        let walk = tokio::spawn(async move {
            execute_job(&ctx, &claimed, admitted(&ctx))
                .await
                .into_outcome()
        });

        // Deterministic seam: the walk is INSIDE iteration 1's NAR
        // fetch of P when the gate reports the hit.
        tokio::time::timeout(std::time::Duration::from_secs(30), hit_rx.recv())
            .await
            .expect("the walk reached tenant A's gated NAR fetch")
            .expect("gate signal");

        // The shift: A's build terminal; B's build live, wanting BOTH
        // outputs ({} = all declared).
        sqlx::query("UPDATE builds SET status = 'succeeded' WHERE build_id = $1")
            .bind(seeded.build_id)
            .execute(&db.pool)
            .await
            .unwrap();
        let build_b = Uuid::new_v4();
        sqlx::query("INSERT INTO builds (build_id, tenant_id, status) VALUES ($1, $2, 'active')")
            .bind(build_b)
            .bind(tenant_b)
            .execute(&db.pool)
            .await
            .unwrap();
        sqlx::query("INSERT INTO build_derivations (build_id, derivation_id) VALUES ($1, $2)")
            .bind(build_b)
            .bind(seeded.derivation_id)
            .execute(&db.pool)
            .await
            .unwrap();
        sqlx::query(
            "INSERT INTO build_wanted_outputs (build_id, derivation_id, wanted_output_names) \
             VALUES ($1, $2, '{}')",
        )
        .bind(build_b)
        .bind(seeded.derivation_id)
        .execute(&db.pool)
        .await
        .unwrap();

        release.notify_waiters();
        let outcome = walk.await.unwrap();

        let success = outcome_success(&outcome).unwrap_or_else(|| {
            panic!(
                "expected Success — iteration 2's new seed must be probed \
                 under the LIVE tenant set (B serves Q), got {outcome:?}"
            )
        });
        let mut got: Vec<String> = success
            .ingested_paths
            .iter()
            .chain(success.verified_paths.iter())
            .cloned()
            .collect();
        got.sort();
        let mut want = vec![path_p.clone(), path_q.clone()];
        want.sort();
        assert_eq!(got, want, "both P (under A) and Q (under B) covered");
    }

    /// bug_266: a per-path verdict settled under an EARLIER, SMALLER
    /// tenant set must be re-probed when the live tenant set GROWS
    /// mid-walk — `new_seeds = wanted - visited` made verdicts
    /// permanent within the walk, so a path that clean-missed under
    /// {A} stayed Unobtainable even when B (whose upstream serves it)
    /// joined before the walk compiled its outcome, violating
    /// owner-Q2 one step removed from the bug_041 close.
    ///
    /// Choreography (the bug_041 seam, with the verdict SETTLING in
    /// iteration 1): build A wants BOTH P and Q; A's gated upstream
    /// serves only P and 404s Q, so Q settles missing-wanted under
    /// generation 1 while P's NAR fetch is blocked; during the block,
    /// A's build goes terminal and B's build (upstream serving Q)
    /// arrives; the gate releases. Iteration 2 re-resolves tenants —
    /// the GROWN set must drain Q's stale verdict back into the
    /// frontier and probe it under B.
    #[tokio::test]
    async fn grown_tenant_set_reprobes_settled_verdicts() {
        let db = TestDb::new(&crate::MIGRATOR).await;
        let tenant_a = seed_tenant(&db.pool, "mat-regrow-a").await;
        let tenant_b = seed_tenant(&db.pool, "mat-regrow-b").await;

        let path_p = store_path(13, "mat-regrow-p");
        let path_q = store_path(14, "mat-regrow-q");
        let (nar_p, _) = rio_test_support::fixtures::make_nar(b"regrow contents p");
        let (nar_q, _) = rio_test_support::fixtures::make_nar(b"regrow contents q");

        // Tenant A: gated upstream serving ONLY P (404s Q).
        let (up_a, mut hit_rx, release) =
            spawn_gated_upstream(vec![(path_p.clone(), nar_p, vec![])], "cache.regrow-a").await;
        wire_upstream(&db.pool, tenant_a, &up_a).await;
        // Tenant B: serves ONLY Q.
        let up_b =
            spawn_multi_upstream(vec![(path_q.clone(), nar_q, vec![])], "cache.regrow-b").await;
        wire_upstream(&db.pool, tenant_b, &up_b).await;

        // Build A wants BOTH outputs ({} = all declared): Q's verdict
        // settles in iteration 1 under {A}.
        let seeded = seed_job(
            &db.pool,
            "mat-regrow-drv",
            &[("out", path_p.as_str()), ("doc", path_q.as_str())],
            Some(tenant_a),
            Some(tenant_a),
            &[],
        )
        .await;

        let ctx = make_ctx(db.pool.clone());
        let claimed = seeded.claimed.clone();
        let walk = tokio::spawn(async move {
            execute_job(&ctx, &claimed, admitted(&ctx))
                .await
                .into_outcome()
        });

        // Deterministic seam: inside iteration 1's gated NAR fetch of
        // P (Q's 404 verdict lands in this iteration either side of
        // the gate — the tenant set was resolved before the gate).
        tokio::time::timeout(std::time::Duration::from_secs(30), hit_rx.recv())
            .await
            .expect("the walk reached tenant A's gated NAR fetch")
            .expect("gate signal");

        // The growth: A terminal; B live, wanting all outputs.
        sqlx::query("UPDATE builds SET status = 'succeeded' WHERE build_id = $1")
            .bind(seeded.build_id)
            .execute(&db.pool)
            .await
            .unwrap();
        let build_b = Uuid::new_v4();
        sqlx::query("INSERT INTO builds (build_id, tenant_id, status) VALUES ($1, $2, 'active')")
            .bind(build_b)
            .bind(tenant_b)
            .execute(&db.pool)
            .await
            .unwrap();
        sqlx::query("INSERT INTO build_derivations (build_id, derivation_id) VALUES ($1, $2)")
            .bind(build_b)
            .bind(seeded.derivation_id)
            .execute(&db.pool)
            .await
            .unwrap();
        sqlx::query(
            "INSERT INTO build_wanted_outputs (build_id, derivation_id, wanted_output_names) \
             VALUES ($1, $2, '{}')",
        )
        .bind(build_b)
        .bind(seeded.derivation_id)
        .execute(&db.pool)
        .await
        .unwrap();

        release.notify_waiters();
        let outcome = walk.await.unwrap();

        let success = outcome_success(&outcome).unwrap_or_else(|| {
            panic!(
                "expected Success — Q's generation-1 miss verdict must be \
                 re-probed under the GROWN tenant set (B serves Q), got \
                 {outcome:?}"
            )
        });
        let mut got: Vec<String> = success
            .ingested_paths
            .iter()
            .chain(success.verified_paths.iter())
            .cloned()
            .collect();
        got.sort();
        got.dedup();
        let mut want = vec![path_p.clone(), path_q.clone()];
        want.sort();
        assert_eq!(got, want, "both P (under A) and Q (under B) covered");
    }

    // r[verify sched.merge.stale-substitutable+3]
    /// bug_027: the carrier union is UNCONDITIONAL — a carried job
    /// whose live interest vanished mid-claim still resolves its
    /// carried realized paths, agreeing with the scheduler's
    /// consumption leg (materialize.rs unions the carrier BEFORE its
    /// width check). Pre-fix, the zero-live-rows early return skipped
    /// the carrier read entirely — the store reported
    /// InfraFailure("no-verifiable-wanted-paths") where the scheduler
    /// computed non-zero width, and the divergence charged the
    /// materialization park budget for the exact race merged_bug_194
    /// closed uncharged.
    #[tokio::test]
    async fn carried_paths_survive_interest_vanishing() {
        let db = TestDb::new(&crate::MIGRATOR).await;
        let tenant = seed_tenant(&db.pool, "mat-carrier").await;
        let carried = store_path(9, "mat-carried");
        let seeded = seed_job(
            &db.pool,
            "mat-carrier-drv",
            &[("out", carried.as_str())],
            Some(tenant),
            Some(tenant),
            &["out"],
        )
        .await;
        // The 082_materialization_job_carried_paths carrier on the
        // job row (stale_reset origin).
        sqlx::query(
            "UPDATE materialization_jobs SET carried_realized_paths = $1 \
              WHERE job_id = $2",
        )
        .bind(vec![carried.clone()])
        .bind(seeded.job_id)
        .execute(&db.pool)
        .await
        .unwrap();
        // Interest vanishes: the only interested build goes terminal.
        sqlx::query("UPDATE builds SET status = 'succeeded' WHERE build_id = $1")
            .bind(seeded.build_id)
            .execute(&db.pool)
            .await
            .unwrap();

        let ctx = make_ctx(db.pool.clone());
        let paths = live_wanted_paths(&ctx, &seeded.claimed).await.unwrap();
        assert_eq!(
            paths,
            vec![carried.clone()],
            "zero live interest must still surface the carried realized \
             paths (the scheduler's consumption leg unions the carrier \
             unconditionally — the two width definitions may not fork)"
        );
    }

    // r[verify store.materialize.tenant-fold+2]
    /// merged_bug_133 red: the recorded (hint) tenant's upstream is
    /// DEAD (every request 500s → charging class), a SECOND
    /// interested tenant's upstream SERVES the path. The hint tenant
    /// sorts first (deterministic resolve order), and the pre-fix
    /// in-loop `ChargeInfra => return infra_failure(...)` aborted the
    /// walk before the serving tenant was ever consulted — violating
    /// owner-Q2 ("a job fails only when NO interested tenant can
    /// obtain"). With the per-tenant evidence cells all failure
    /// dispositions move to the post-loop fold: B serves → Success.
    ///
    /// Recorded red (pre-fix): `expected Success via the second
    /// tenant, got InfraFailure("substitution of … failed (Fetch)")`.
    #[tokio::test]
    async fn dead_first_tenant_then_serving_second_succeeds() {
        let db = TestDb::new(&crate::MIGRATOR).await;
        let dead_tenant = seed_tenant(&db.pool, "mat-dead-a").await;
        let serving_tenant = seed_tenant(&db.pool, "mat-serve-b").await;

        let path = store_path(7, "mat-dead-then-serve");
        let (nar, _) = rio_test_support::fixtures::make_nar(b"served by tenant B");
        // Tenant A (the job hint → consulted FIRST): all-500 upstream
        // → all-errored iteration → charging class.
        let dead = spawn_status_upstream(axum::http::StatusCode::INTERNAL_SERVER_ERROR).await;
        wire_upstream(&db.pool, dead_tenant, &dead).await;
        // Tenant B: serves the path.
        let upstream =
            spawn_multi_upstream(vec![(path.clone(), nar, vec![])], "cache.deadab").await;
        wire_upstream(&db.pool, serving_tenant, &upstream).await;

        // Creating build (ACTIVE) carries the dead tenant → live
        // interest AND the hint. A second live build carries the
        // serving tenant.
        let seeded = seed_job(
            &db.pool,
            "mat-dead-then-serve-drv",
            &[("out", path.as_str())],
            Some(dead_tenant),
            Some(dead_tenant),
            &[],
        )
        .await;
        let live_build = Uuid::new_v4();
        sqlx::query("INSERT INTO builds (build_id, tenant_id, status) VALUES ($1, $2, 'active')")
            .bind(live_build)
            .bind(serving_tenant)
            .execute(&db.pool)
            .await
            .unwrap();
        sqlx::query("INSERT INTO build_derivations (build_id, derivation_id) VALUES ($1, $2)")
            .bind(live_build)
            .bind(seeded.derivation_id)
            .execute(&db.pool)
            .await
            .unwrap();
        sqlx::query(
            "INSERT INTO build_wanted_outputs (build_id, derivation_id, wanted_output_names) \
             VALUES ($1, $2, '{}')",
        )
        .bind(live_build)
        .bind(seeded.derivation_id)
        .execute(&db.pool)
        .await
        .unwrap();

        let ctx = make_ctx(db.pool.clone());
        let outcome = execute_job(&ctx, &seeded.claimed, admitted(&ctx))
            .await
            .into_outcome();
        let success = outcome_success(&outcome).unwrap_or_else(|| {
            panic!(
                "expected Success via the second (serving) tenant, got {outcome:?} — \
                 the first tenant's charging failure starved the iteration"
            )
        });
        assert_eq!(
            success.ingested_paths.len(),
            1,
            "the wanted path was ingested under the serving tenant"
        );
    }

    // r[verify store.materialize.tenant-fold+2]
    /// merged_bug_188: a placeholder race is PATH-keyed (tenant-
    /// independent) — once one tenant's attempt answers Raced, the
    /// remaining tenants would race the same held slot, and a sibling
    /// tenant's pre-claim charging failure (narinfo 500s before the
    /// claim) must not convert the uncharged race into a job-fatal
    /// charge via the fold's Charge-dominates precedence. The tenant
    /// loop aborts on Raced (Transient cell recorded first, so the
    /// uncharged deferral survives the fold).
    #[tokio::test]
    async fn raced_first_tenant_defers_uncharged_despite_charging_sibling() {
        let db = TestDb::new(&crate::MIGRATOR).await;
        let raced_tenant = seed_tenant(&db.pool, "m188-raced-a").await;
        let charge_tenant = seed_tenant(&db.pool, "m188-charge-b").await;

        let path = store_path(27, "m188-raced-slot");
        let (nar, _) = rio_test_support::fixtures::make_nar(b"m188 raced slot");
        // Tenant A (the hint, consulted FIRST): healthy upstream, so
        // its attempt reaches the placeholder claim — which answers
        // Concurrent (young 'uploading' manifest) → Raced.
        let healthy = spawn_multi_upstream(vec![(path.clone(), nar, vec![])], "cache.m188a").await;
        wire_upstream(&db.pool, raced_tenant, &healthy).await;
        // Tenant B: all-500 upstream → pre-claim charging class.
        let dead = spawn_status_upstream(axum::http::StatusCode::INTERNAL_SERVER_ERROR).await;
        wire_upstream(&db.pool, charge_tenant, &dead).await;

        // The path-keyed placeholder held by a concurrent uploader.
        let sp = StorePath::parse(&path).unwrap();
        let hash = sp.sha256_digest();
        crate::metadata::insert_manifest_uploading(&db.pool, &hash, &path, &[])
            .await
            .unwrap();

        let seeded = seed_job(
            &db.pool,
            "m188-raced-drv",
            &[("out", path.as_str())],
            Some(raced_tenant),
            Some(raced_tenant),
            &[],
        )
        .await;
        let live_build = Uuid::new_v4();
        sqlx::query("INSERT INTO builds (build_id, tenant_id, status) VALUES ($1, $2, 'active')")
            .bind(live_build)
            .bind(charge_tenant)
            .execute(&db.pool)
            .await
            .unwrap();
        sqlx::query("INSERT INTO build_derivations (build_id, derivation_id) VALUES ($1, $2)")
            .bind(live_build)
            .bind(seeded.derivation_id)
            .execute(&db.pool)
            .await
            .unwrap();
        sqlx::query(
            "INSERT INTO build_wanted_outputs (build_id, derivation_id, wanted_output_names) \
             VALUES ($1, $2, '{}')",
        )
        .bind(live_build)
        .bind(seeded.derivation_id)
        .execute(&db.pool)
        .await
        .unwrap();

        let ctx = make_ctx(db.pool.clone());
        let outcome = execute_job(&ctx, &seeded.claimed, admitted(&ctx))
            .await
            .into_outcome();
        let retry = outcome_retry_later(&outcome).unwrap_or_else(|| {
            panic!(
                "expected uncharged RetryLater (the race defers; the sibling's \
                 charging failure must not dominate a path-keyed race), got {outcome:?}"
            )
        });
        assert_eq!(retry.class, "raced");
    }

    // r[verify store.materialize.executor+5]
    /// (5) A confirmed-404 wanted path (every upstream definitively
    /// answers "not present") → Unobtainable{missing_paths=[it]},
    /// with whatever WAS obtained in verified_paths.
    #[tokio::test]
    async fn confirmed_missing_reports_unobtainable() {
        let db = TestDb::new(&crate::MIGRATOR).await;
        let tenant = seed_tenant(&db.pool, "mat-missing").await;

        let present = store_path(7, "mat-present");
        let absent = store_path(8, "mat-absent");
        let (present_nar, _) = rio_test_support::fixtures::make_nar(b"present");
        // The upstream serves `present` but NOT `absent` → 404 for it.
        let upstream =
            spawn_multi_upstream(vec![(present.clone(), present_nar, vec![])], "cache.miss").await;
        wire_upstream(&db.pool, tenant, &upstream).await;

        let seeded = seed_job(
            &db.pool,
            "mat-missing-drv",
            &[("out", present.as_str()), ("doc", absent.as_str())],
            Some(tenant),
            Some(tenant),
            &[],
        )
        .await;

        let ctx = make_ctx(db.pool.clone());
        let outcome = execute_job(&ctx, &seeded.claimed, admitted(&ctx))
            .await
            .into_outcome();

        let unobtainable = outcome_unobtainable(&outcome)
            .unwrap_or_else(|| panic!("expected Unobtainable, got {outcome:?}"));
        assert_eq!(
            unobtainable.missing_paths,
            vec![absent.clone()],
            "the confirmed-absent path is reported missing"
        );
        assert_eq!(
            unobtainable.verified_paths,
            vec![present.clone()],
            "what WAS obtained rides the same report"
        );
        assert!(
            !unobtainable.cause.is_empty(),
            "the cause string is populated"
        );
    }

    /// merged_bug_005: a path that is PRESENT upstream but whose
    /// narinfo signature fails the tenant's `trusted_keys` (rotated /
    /// mistyped entry) must settle **Unobtainable** — the uncharged
    /// from-source settlement — with a cause naming the trust
    /// refusal. Pre-fix the sig-refusal folded to `CleanMiss`
    /// ("as good as 404"), the sig-blind HEAD confirmation then saw
    /// the path present, and every claim charged InfraFailure
    /// ("present but not ingested") — parking the job at
    /// max_attempts forever instead of settling.
    #[tokio::test]
    async fn sig_untrusted_present_settles_unobtainable() {
        use base64::Engine;
        let db = TestDb::new(&crate::MIGRATOR).await;
        let tenant = seed_tenant(&db.pool, "mat-untrusted").await;

        let path = store_path(33, "mat-untrusted");
        let (nar, _) = rio_test_support::fixtures::make_nar(b"untrusted");
        // The upstream signs with one key; the tenant trusts the SAME
        // key name with a DIFFERENT public key (the rotated-key shape).
        let upstream =
            spawn_multi_upstream(vec![(path.clone(), nar, vec![])], "cache.rotated").await;
        let wrong_pub = ed25519_dalek::SigningKey::from_bytes(&[0x77u8; 32]).verifying_key();
        let wrong_key = format!(
            "cache.rotated:{}",
            base64::engine::general_purpose::STANDARD.encode(wrong_pub.as_bytes())
        );
        metadata::upstreams::insert(
            &db.pool,
            tenant,
            &upstream.url,
            50,
            std::slice::from_ref(&wrong_key),
            SigMode::Keep,
        )
        .await
        .expect("upstream wired with rotated trusted key");

        let seeded = seed_job(
            &db.pool,
            "mat-untrusted-drv",
            &[("out", path.as_str())],
            Some(tenant),
            Some(tenant),
            &[],
        )
        .await;

        let ctx = make_ctx(db.pool.clone());
        let outcome = execute_job(&ctx, &seeded.claimed, admitted(&ctx))
            .await
            .into_outcome();

        let unobtainable = outcome_unobtainable(&outcome).unwrap_or_else(|| {
            panic!("present-but-untrusted must settle Unobtainable (uncharged), got {outcome:?}")
        });
        assert_eq!(
            unobtainable.missing_paths,
            vec![path.clone()],
            "the trust-refused path rides the missing-wanted cell"
        );
        assert!(
            unobtainable.cause.contains("signature"),
            "the cause names the trust refusal, not a generic miss: {}",
            unobtainable.cause
        );
    }

    /// merged_bug_016: a positive probe-cache entry (1h TTL, written
    /// by the dispatch-time HEAD probe) must not override the
    /// execution-time GETs' fresh 404s. Pre-fix nothing invalidated
    /// the entry when the attempt leg observed the upstream evicted
    /// the path, so `probe_miss`'s confirmation read the STALE cached
    /// `true` → "present but not ingested" → InfraFailure on every
    /// retry until the TTL lapsed — burning the park budget instead
    /// of settling confirmed-absent.
    #[tokio::test]
    async fn stale_probe_cache_positive_does_not_charge_after_eviction() {
        let db = TestDb::new(&crate::MIGRATOR).await;
        let tenant = seed_tenant(&db.pool, "mat-evicted").await;

        let path = store_path(34, "mat-evicted");
        let (nar, _) = rio_test_support::fixtures::make_nar(b"evicted");
        let (upstream, gone) =
            spawn_flippable_upstream(vec![(path.clone(), nar, vec![])], "cache.evict").await;
        wire_upstream(&db.pool, tenant, &upstream).await;

        let ctx = make_ctx(db.pool.clone());
        // Dispatch-time probe: HEAD 200 → probe_cache stores the
        // positive (the very probe that spawns the job in production
        // — the executor shares the same Substituter).
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(10);
        let probed = ctx
            .substituter
            .check_available(tenant, std::slice::from_ref(&path), deadline)
            .await
            .expect("dispatch-time probe");
        assert!(
            probed.hits.iter().any(|p| p == &path),
            "the probe primed the positive cache entry"
        );

        // The upstream evicts the path before execution claims it.
        gone.store(true, std::sync::atomic::Ordering::SeqCst);

        let seeded = seed_job(
            &db.pool,
            "mat-evicted-drv",
            &[("out", path.as_str())],
            Some(tenant),
            Some(tenant),
            &[],
        )
        .await;
        let outcome = execute_job(&ctx, &seeded.claimed, admitted(&ctx))
            .await
            .into_outcome();

        let unobtainable = outcome_unobtainable(&outcome).unwrap_or_else(|| {
            panic!(
                "fresh all-404 GETs must outrank the stale cached positive \
                 (settle Unobtainable), got {outcome:?}"
            )
        });
        assert_eq!(
            unobtainable.missing_paths,
            vec![path.clone()],
            "the evicted path settles confirmed-absent"
        );
    }

    // r[verify store.materialize.executor+5]
    // r[verify store.substitute.stall-abort+2]
    /// (6b) A WEDGED upstream download (headers, then no body bytes)
    /// is ended by the substituter's owner-side stall abort and
    /// surfaces to the executor as **InfraFailure** — the retryable
    /// infrastructure class that feeds the scheduler's materialization
    /// re-attempt budget. Never Unobtainable (nothing confirmed
    /// absent), never Success, never a hang: the stall abort is the
    /// only clock on the NAR body.
    #[tokio::test]
    async fn stalled_download_reports_infra_failure() {
        use axum::{Router, routing::get};
        use base64::Engine;
        use sha2::Digest;

        let db = TestDb::new(&crate::MIGRATOR).await;
        let tenant = seed_tenant(&db.pool, "mat-stall").await;

        // A signed narinfo whose NAR endpoint never sends body bytes.
        let path = store_path(12, "mat-stalled");
        let (nar, _) = rio_test_support::fixtures::make_nar(b"never arrives");
        let seed = [0x42u8; 32];
        let signer = Signer::from_seed("cache.stall", &seed);
        let pubkey = ed25519_dalek::SigningKey::from_bytes(&seed).verifying_key();
        let trusted_key = format!(
            "cache.stall:{}",
            base64::engine::general_purpose::STANDARD.encode(pubkey.as_bytes())
        );
        let nar_hash: [u8; 32] = sha2::Sha256::digest(&nar).into();
        let nar_hash_str = format!(
            "sha256:{}",
            rio_nix::store_path::nixbase32::encode(&nar_hash)
        );
        let fp = fingerprint(&path, &nar_hash, nar.len() as u64, &[]);
        let sig = signer.sign(&fp);
        let sp = StorePath::parse(&path).unwrap();
        let hash_part = sp.hash_part();
        let narinfo = format!(
            "StorePath: {path}\n\
             URL: nar/{hash_part}.nar\n\
             Compression: none\n\
             NarHash: {nar_hash_str}\n\
             NarSize: {}\n\
             References: \n\
             Sig: {sig}\n",
            nar.len()
        );
        let app = Router::new()
            .route(
                "/nix-cache-info",
                get(|| async { "StoreDir: /nix/store\nWantMassQuery: 1\nPriority: 40\n" }),
            )
            .route(
                &format!("/{hash_part}.narinfo"),
                get(move || async move { narinfo }),
            )
            .route(
                &format!("/nar/{hash_part}.nar"),
                get(|| async {
                    tokio::time::sleep(std::time::Duration::from_secs(3600)).await;
                    Vec::<u8>::new()
                }),
            );
        let listener = tokio::net::TcpListener::bind(SocketAddr::from(([127, 0, 0, 1], 0)))
            .await
            .unwrap();
        let addr = listener.local_addr().unwrap();
        let _task = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        metadata::upstreams::insert(
            &db.pool,
            tenant,
            &format!("http://{addr}"),
            50,
            std::slice::from_ref(&trusted_key),
            SigMode::Keep,
        )
        .await
        .expect("upstream wired");

        let seeded = seed_job(
            &db.pool,
            "mat-stall-drv",
            &[("out", path.as_str())],
            Some(tenant),
            Some(tenant),
            &[],
        )
        .await;

        // 1s stall window so the abort fires within the test budget.
        let ctx = ExecutorContext::new(
            db.pool.clone(),
            std::sync::Arc::new(
                Substituter::new(db.pool.clone(), None)
                    .with_http_client(sandbox_http())
                    .with_stall_window(std::time::Duration::from_secs(1)),
            ),
            1,
            PathSlotPool::new(32),
        );
        let outcome = tokio::time::timeout(std::time::Duration::from_secs(30), async {
            execute_job(&ctx, &seeded.claimed, admitted(&ctx))
                .await
                .into_outcome()
        })
        .await
        .expect("the stall abort must end the wedged download (no hang)");

        let infra = outcome_infra(&outcome).unwrap_or_else(|| {
            panic!("a stalled download must classify InfraFailure, got {outcome:?}")
        });
        assert!(
            infra.detail.contains("stalled"),
            "the detail names the stall: {}",
            infra.detail
        );
        assert_eq!(
            pin_count(&db.pool, "mat-stall-drv", "materialization").await,
            0,
            "nothing is pinned when nothing is ingested"
        );
    }

    // r[verify store.materialize.executor+5]
    /// (6) Upstream 5xx → InfraFailure (B3's executor half): nothing is
    /// confirmed, so the verdict must be infrastructure trouble — never
    /// Unobtainable (which would route from-source), never Success.
    #[tokio::test]
    async fn upstream_5xx_reports_infra_failure() {
        let db = TestDb::new(&crate::MIGRATOR).await;
        let tenant = seed_tenant(&db.pool, "mat-5xx").await;

        let path = store_path(9, "mat-5xx");
        let upstream = spawn_status_upstream(axum::http::StatusCode::INTERNAL_SERVER_ERROR).await;
        wire_upstream(&db.pool, tenant, &upstream).await;

        let seeded = seed_job(
            &db.pool,
            "mat-5xx-drv",
            &[("out", path.as_str())],
            Some(tenant),
            Some(tenant),
            &[],
        )
        .await;

        let ctx = make_ctx(db.pool.clone());
        let outcome = execute_job(&ctx, &seeded.claimed, admitted(&ctx))
            .await
            .into_outcome();

        let infra = outcome_infra(&outcome).unwrap_or_else(|| {
            panic!("expected InfraFailure for a 5xx-only upstream, got {outcome:?}")
        });
        assert!(
            !infra.detail.is_empty(),
            "the infra detail is populated: {infra:?}"
        );
        assert_eq!(
            pin_count(&db.pool, "mat-5xx-drv", "materialization").await,
            0,
            "nothing is pinned when nothing is confirmed"
        );
    }

    // r[verify store.materialize.executor+5]
    /// (7) T-1.2 / BC-4: the executor reports cumulative, monotone byte
    /// progress through the callback while walking a multi-path closure.
    /// Every call satisfies done ≤ expected; done never decreases; the
    /// final call's done equals the sum of the closure's NAR sizes.
    #[tokio::test]
    async fn execution_reports_cumulative_monotone_progress() {
        let db = TestDb::new(&crate::MIGRATOR).await;
        let tenant = seed_tenant(&db.pool, "mat-progress").await;

        // A two-node closure: root references dep (two NARs of
        // different sizes so the cumulative accounting is visible).
        let root = store_path(10, "mat-progress-root");
        let dep = store_path(11, "mat-progress-dep");
        // The root NAR spans several progress intervals (but stays
        // under the cfg(test) decompressed cap) so IN-FLIGHT ticks
        // fire (merged_bug_195: the strengthened pin below requires a
        // tick where expected leads done, with the serving upstream
        // named).
        let big =
            vec![0x5au8; (crate::substitute::SUBSTITUTE_PROGRESS_INTERVAL_BYTES * 3) as usize];
        let (root_nar, _) = rio_test_support::fixtures::make_nar(&big);
        let (dep_nar, _) = rio_test_support::fixtures::make_nar(b"dep");
        let total_nar_bytes = (root_nar.len() + dep_nar.len()) as u64;
        let upstream = spawn_multi_upstream(
            vec![
                (root.clone(), root_nar, vec![dep.clone()]),
                (dep.clone(), dep_nar, vec![]),
            ],
            "cache.progress",
        )
        .await;
        wire_upstream(&db.pool, tenant, &upstream).await;

        let seeded = seed_job(
            &db.pool,
            "mat-progress-drv",
            &[("out", root.as_str())],
            Some(tenant),
            Some(tenant),
            &[],
        )
        .await;

        let ctx = make_ctx(db.pool.clone());
        // The callback is `'static` now (the per-path adapters own a
        // handle, merged_bug_195) — Arc the collector.
        let calls: std::sync::Arc<std::sync::Mutex<Vec<(u64, u64, String)>>> =
            std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let calls_cb = calls.clone();
        let outcome = execute_job_with_progress(
            &ctx,
            &seeded.claimed,
            admitted(&ctx),
            move |done, expected, uri| {
                calls_cb
                    .lock()
                    .unwrap()
                    .push((done, expected, uri.to_string()));
            },
        )
        .await
        .into_outcome();

        outcome_success(&outcome).unwrap_or_else(|| panic!("expected Success, got {outcome:?}"));

        let calls = std::sync::Arc::try_unwrap(calls)
            .expect("walk done; no other handle")
            .into_inner()
            .unwrap();
        assert!(
            !calls.is_empty(),
            "the walk must report progress through the callback (BC-4 relay source)"
        );
        let mut prev_done = 0u64;
        for (done, expected, _) in &calls {
            assert!(
                *done >= prev_done,
                "bytes_done must be monotone non-decreasing: {prev_done} then {done} in {calls:?}"
            );
            assert!(
                done <= expected,
                "bytes_done must never exceed bytes_expected: ({done}, {expected}) in {calls:?}"
            );
            prev_done = *done;
        }
        // merged_bug_195 (strengthened): the in-flight layer is WIRED —
        // at least one mid-fetch tick has expected genuinely leading
        // done (the declared NarSize precedes the body) and names the
        // serving upstream. Pre-fix every tick was the degenerate
        // (done == expected, "") pair.
        assert!(
            calls
                .iter()
                .any(|(done, expected, uri)| done < expected && !uri.is_empty()),
            "an in-flight tick must lead done and name the upstream: {calls:?}"
        );
        let (final_done, final_expected, _) = calls.last().expect("non-empty");
        assert_eq!(
            *final_done, total_nar_bytes,
            "the final report's done covers the whole closure ({calls:?})"
        );
        assert_eq!(
            *final_expected, total_nar_bytes,
            "the final report's expected equals the closure total ({calls:?})"
        );
    }

    // r[verify store.materialize.progress-monotone+1]
    /// R1-012 (merged_bug_012, TRUE RED pre-fix) / W-012a: over a
    /// trace where a sibling commit drives the floor past a
    /// still-streaming path's declared size, NO provisional emission
    /// may render complete (`done == expected == post-commit floor`)
    /// while that path is still mid-fetch. Two independent wanted
    /// paths at F = 4, both spawned at job start: X (large) commits
    /// first; Y (small, gated until X's commit) then streams.
    ///
    /// Pre-fix red: every Y tick emitted (base + streamed, base +
    /// declared) = (streamed, declared) with base = 0, and the clamp
    /// dragged both to the floor — (floor, floor) frames: a false
    /// 100% bar mid-fetch, sawtoothing at each commit. Post-fix: the
    /// driver fold emits (floor + Σ streamed, floor + Σ declared) —
    /// expected genuinely leads done until Y's body completes.
    #[tokio::test]
    async fn floor_passing_a_streaming_base_does_not_render_complete() {
        let db = TestDb::new(&crate::MIGRATOR).await;
        let tenant = seed_tenant(&db.pool, "fold-floor").await;

        let interval = crate::substitute::SUBSTITUTE_PROGRESS_INTERVAL_BYTES as usize;
        let x = store_path(100, "foldfloor-x");
        let y = store_path(101, "foldfloor-y");
        // Under the cfg(test) 64 KiB decompressed cap: X = 3 intervals
        // (48 KiB), Y = 2 (32 KiB); floor after X (~49K) > Y declared.
        let (x_nar, _) = rio_test_support::fixtures::make_nar(&vec![0x6au8; interval * 3]);
        let (y_nar, _) = rio_test_support::fixtures::make_nar(&vec![0x6bu8; interval * 2]);
        let x_len = x_nar.len() as u64;
        let (upstream, gates) = spawn_pathgated_upstream(
            vec![(x.clone(), x_nar, vec![]), (y.clone(), y_nar, vec![])],
            "cache.foldfloor",
        )
        .await;
        wire_upstream(&db.pool, tenant, &upstream).await;
        let seeded = seed_job(
            &db.pool,
            "foldfloor-drv",
            &[("ox", x.as_str()), ("oy", y.as_str())],
            Some(tenant),
            Some(tenant),
            &[],
        )
        .await;

        let events: std::sync::Arc<std::sync::Mutex<Vec<(u64, u64, String)>>> =
            std::sync::Arc::default();
        let sink = std::sync::Arc::clone(&events);
        let ctx = make_ctx(db.pool.clone()); // F = 4: both in flight
        let job = tokio::spawn({
            let claimed = seeded.claimed.clone();
            let admission = admitted(&ctx);
            async move {
                execute_job_with_progress(&ctx, &claimed, admission, move |d, e, u| {
                    sink.lock().unwrap().push((d, e, u.to_string()));
                })
                .await
                .into_outcome()
            }
        });

        // Release X; hold Y until X's commit raises the floor past
        // Y's declared size (floor = x_len > y declared).
        let gx = std::sync::Arc::clone(&gates[&x]);
        tokio::spawn(async move {
            for _ in 0..400 {
                gx.notify_one();
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        });
        let committed =
            |evs: &[(u64, u64, String)]| evs.iter().filter(|(_, _, u)| u.is_empty()).count();
        let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
        while committed(&events.lock().unwrap()) < 1 {
            assert!(
                tokio::time::Instant::now() < deadline,
                "X's commit never landed"
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        let gy = std::sync::Arc::clone(&gates[&y]);
        tokio::spawn(async move {
            for _ in 0..400 {
                gy.notify_one();
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        });
        let outcome = tokio::time::timeout(Duration::from_secs(30), job)
            .await
            .expect("walk completes")
            .unwrap();
        outcome_success(&outcome).unwrap_or_else(|| panic!("expected Success, got {outcome:?}"));

        // Between X's commit (the first uri == "" event) and Y's
        // commit (the second), NO provisional frame may read
        // done == expected == the post-X floor (the clamped
        // false-complete cell).
        let evs = events.lock().unwrap().clone();
        let first_commit = evs
            .iter()
            .position(|(_, _, u)| u.is_empty())
            .expect("X committed");
        let second_commit = evs
            .iter()
            .enumerate()
            .filter(|(_, (_, _, u))| u.is_empty())
            .map(|(i, _)| i)
            .nth(1)
            .expect("Y committed");
        let false_complete: Vec<&(u64, u64, String)> = evs[first_commit + 1..second_commit]
            .iter()
            .filter(|(d, e, u)| !u.is_empty() && d == e && *d == x_len)
            .collect();
        assert!(
            false_complete.is_empty(),
            "provisional frames rendered complete at the post-commit floor while \
             Y was still streaming (false-100% mid-fetch): {false_complete:?} \
             (full trace: {evs:?})"
        );
    }

    /// R2-012 fixture: serves each path's NAR as a PACED chunked body
    /// — one 16 KiB chunk per scheduled delay — so two siblings
    /// genuinely stream CONCURRENTLY with a controlled offset (the
    /// full-body-after-gate harnesses complete in microseconds on
    /// loopback and never overlap mid-stream).
    async fn spawn_paced_chunked_upstream(
        paths: Vec<(String, Vec<u8>, Vec<Duration>)>,
        key_name: &str,
    ) -> FakeUpstream {
        use axum::{Router, routing::get};
        use base64::Engine;
        use futures_util::StreamExt as _;
        use sha2::Digest;

        let seed = [0x47u8; 32];
        let signer = Signer::from_seed(key_name, &seed);
        let pubkey = ed25519_dalek::SigningKey::from_bytes(&seed).verifying_key();
        let trusted_key = format!(
            "{key_name}:{}",
            base64::engine::general_purpose::STANDARD.encode(pubkey.as_bytes())
        );
        let mut app = Router::new().route(
            "/nix-cache-info",
            get(|| async { "StoreDir: /nix/store\nWantMassQuery: 1\nPriority: 40\n" }),
        );
        for (path, nar, schedule) in paths {
            let nar_hash: [u8; 32] = sha2::Sha256::digest(&nar).into();
            let nar_hash_str = format!(
                "sha256:{}",
                rio_nix::store_path::nixbase32::encode(&nar_hash)
            );
            let fp = fingerprint(&path, &nar_hash, nar.len() as u64, &[]);
            let sig = signer.sign(&fp);
            let sp = StorePath::parse(&path).unwrap();
            let hash_part = sp.hash_part();
            let narinfo = format!(
                "StorePath: {path}\n\
                 URL: nar/{hash_part}.nar\n\
                 Compression: none\n\
                 NarHash: {nar_hash_str}\n\
                 NarSize: {}\n\
                 References: \n\
                 Sig: {sig}\n",
                nar.len(),
            );
            let chunks: Vec<(Vec<u8>, Duration)> = nar
                .chunks(16 * 1024)
                .map(|c| c.to_vec())
                .zip(
                    schedule
                        .into_iter()
                        .chain(std::iter::repeat(Duration::ZERO)),
                )
                .collect();
            app = app
                .route(
                    &format!("/{hash_part}.narinfo"),
                    get(move || async move { narinfo }),
                )
                .route(
                    &format!("/nar/{hash_part}.nar"),
                    get(move || async move {
                        let stream =
                            futures_util::stream::iter(chunks).then(|(chunk, delay)| async move {
                                tokio::time::sleep(delay).await;
                                Ok::<_, std::convert::Infallible>(axum::body::Bytes::from(chunk))
                            });
                        axum::body::Body::from_stream(stream)
                    }),
                );
        }
        let listener = tokio::net::TcpListener::bind(SocketAddr::from(([127, 0, 0, 1], 0)))
            .await
            .unwrap();
        let addr = listener.local_addr().unwrap();
        let task = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        FakeUpstream {
            url: format!("http://{addr}"),
            trusted_key,
            _task: task,
        }
    }

    // r[verify store.materialize.progress-monotone+1]
    /// R2-012 (merged_bug_012, TRUE RED pre-fix) / W-012b: the emitted
    /// `done` sequence on a SUCCESS-ONLY trace is non-decreasing —
    /// each successive provisional differs from its predecessor by one
    /// path's streamed delta or an apply boundary, so cross-sibling
    /// oscillation (the 300K → 2K → 320K shape) is unrepresentable in
    /// the emitted sequence. Asserted over the captured event vec,
    /// not wall-clock. (The licensed step-back after a FAILED attempt
    /// is out of this trace's scope — success-only; W-012c covers it.)
    ///
    /// Pre-fix red: two concurrently-streaming siblings of different
    /// sizes emitted base-adjusted pairs whose interleave regressed
    /// `done` by the inter-sibling stream gap on alternating frames.
    #[tokio::test]
    async fn sibling_ticks_do_not_oscillate_the_wire_pair() {
        let db = TestDb::new(&crate::MIGRATOR).await;
        let tenant = seed_tenant(&db.pool, "fold-osc").await;

        let interval = crate::substitute::SUBSTITUTE_PROGRESS_INTERVAL_BYTES as usize;
        let a = store_path(102, "foldosc-a");
        let b = store_path(103, "foldosc-b");
        // A streams two fast chunks then a LONG tail; B's first chunk
        // lands inside A's tail — A's raw done (32K) is ahead of B's
        // first tick (16K), so the pre-fix interleave regresses the
        // wire pair while BOTH are mid-stream (no commit yet, so the
        // clamp cannot mask it). Sizes under the cfg(test) 64 KiB cap.
        let (a_nar, _) = rio_test_support::fixtures::make_nar(&vec![0x6cu8; interval * 3]);
        let (b_nar, _) = rio_test_support::fixtures::make_nar(&vec![0x6du8; interval * 2]);
        let upstream = spawn_paced_chunked_upstream(
            vec![
                (
                    a.clone(),
                    a_nar,
                    vec![
                        Duration::from_millis(10),
                        Duration::from_millis(10),
                        Duration::from_millis(400),
                    ],
                ),
                (
                    b.clone(),
                    b_nar,
                    vec![Duration::from_millis(120), Duration::from_millis(10)],
                ),
            ],
            "cache.foldosc",
        )
        .await;
        wire_upstream(&db.pool, tenant, &upstream).await;
        let seeded = seed_job(
            &db.pool,
            "foldosc-drv",
            &[("oa", a.as_str()), ("ob", b.as_str())],
            Some(tenant),
            Some(tenant),
            &[],
        )
        .await;

        let events: std::sync::Arc<std::sync::Mutex<Vec<(u64, u64, String)>>> =
            std::sync::Arc::default();
        let sink = std::sync::Arc::clone(&events);
        let ctx = make_ctx(db.pool.clone()); // F = 4: both stream concurrently
        let job = tokio::spawn({
            let claimed = seeded.claimed.clone();
            let admission = admitted(&ctx);
            async move {
                execute_job_with_progress(&ctx, &claimed, admission, move |d, e, u| {
                    sink.lock().unwrap().push((d, e, u.to_string()));
                })
                .await
                .into_outcome()
            }
        });
        // No gates: the paced bodies overlap by construction.
        let outcome = tokio::time::timeout(Duration::from_secs(30), job)
            .await
            .expect("walk completes")
            .unwrap();
        outcome_success(&outcome).unwrap_or_else(|| panic!("expected Success, got {outcome:?}"));

        let evs = events.lock().unwrap().clone();
        // Sanity: provisional ticks flowed.
        assert!(
            evs.iter().any(|(_, _, u)| !u.is_empty()),
            "provisional ticks flowed"
        );
        // W-012b over the PROVISIONAL subsequence: `done` is
        // non-decreasing — cross-sibling oscillation is
        // unrepresentable. Commit frames (uri == \"\") are APPLY
        // BOUNDARIES, allowed steps by the law (they emit the
        // floor-only pair — the recorded divergence (b): commit-frame
        // shape is out of defect scope); the boundary's effect on the
        // next provisional is sign-checked separately below.
        let mut prev = 0u64;
        for (i, (d, _, u)) in evs.iter().enumerate() {
            if u.is_empty() {
                continue;
            }
            assert!(
                *d >= prev,
                "emitted done regressed at provisional frame {i}: {prev} -> {d} \
                 (the cross-sibling oscillation cell; trace: {evs:?})"
            );
            prev = *d;
        }
        // Across an apply boundary the provisional aggregate never
        // shrinks either (retire trades streamed-bytes for the
        // committed nar size): the FULL provisional run is monotone.
        let (final_done, final_expected, _) = evs.last().expect("non-empty");
        assert_eq!(
            final_done, final_expected,
            "the final frame covers the closure total"
        );
    }

    // r[verify obs.metric.store]
    // r[verify store.materialize.executor+5]
    /// T-6.2 (red-first): the execution-outcome and pin counters the
    /// dashboards consume —
    ///
    ///   rio_store_materialization_executions_total{outcome}: one
    ///     increment per finished job execution, labeled
    ///     success | unobtainable | infra;
    ///   rio_store_materialization_pinned_paths_total: one increment
    ///     per path pinned at ingest (the §5.3 pin lifecycle's supply
    ///     side).
    #[tokio::test]
    async fn execution_metrics_count_outcomes_and_pins() {
        use metrics_util::debugging::DebuggingRecorder;
        let rec = DebuggingRecorder::new();
        let snap = rec.snapshotter();
        let _guard = metrics::set_default_local_recorder(&rec);

        let db = TestDb::new(&crate::MIGRATOR).await;
        let tenant = seed_tenant(&db.pool, "mat-metrics").await;

        // A two-node closure: root references dep (both pinned on
        // success → pinned_paths_total = 2).
        let root = store_path(7, "matm-root");
        let dep = store_path(8, "matm-dep");
        let (root_nar, _) = rio_test_support::fixtures::make_nar(b"matm root");
        let (dep_nar, _) = rio_test_support::fixtures::make_nar(b"matm dep");
        let upstream = spawn_multi_upstream(
            vec![
                (root.clone(), root_nar, vec![dep.clone()]),
                (dep.clone(), dep_nar, vec![]),
            ],
            "cache.metrics",
        )
        .await;
        wire_upstream(&db.pool, tenant, &upstream).await;
        let seeded = seed_job(
            &db.pool,
            "matm-drv",
            &[("out", root.as_str())],
            Some(tenant),
            Some(tenant),
            &[],
        )
        .await;
        let ctx = make_ctx(db.pool.clone());
        let outcome = execute_job(&ctx, &seeded.claimed, admitted(&ctx))
            .await
            .into_outcome();
        assert!(
            outcome_success(&outcome).is_some(),
            "precondition: the execution succeeds, got {outcome:?}"
        );

        // The counters: one success execution, two pinned paths.
        use metrics_util::debugging::DebugValue;
        let mut executions: std::collections::BTreeMap<String, u64> = Default::default();
        let mut pinned: u64 = 0;
        for (ck, _, _, v) in snap.snapshot().into_vec() {
            let DebugValue::Counter(c) = v else { continue };
            let k = ck.key();
            match k.name() {
                "rio_store_materialization_executions_total" => {
                    let outcome_label = k
                        .labels()
                        .find(|l| l.key() == "outcome")
                        .map(|l| l.value().to_owned())
                        .unwrap_or_default();
                    *executions.entry(outcome_label).or_default() += c;
                }
                "rio_store_materialization_pinned_paths_total" => pinned += c,
                _ => {}
            }
        }
        assert_eq!(
            executions.get("success").copied().unwrap_or(0),
            1,
            "one success execution counted; executions: {executions:?}"
        );
        assert_eq!(
            pinned, 2,
            "both closure members pinned at ingest are counted"
        );
    }

    /// merged_bug_176 (bughunt wave): tenant re-resolution discovers a
    /// LIVE build that is a member (`build_derivations`) but has no
    /// `build_wanted_outputs` row. Pre-fix the query joined the wanted
    /// relation, so a row-less live build's tenant was invisible and
    /// the executor reported InfraFailure ("no tenant context") for a
    /// job a live build genuinely wants.
    #[tokio::test]
    async fn resolve_tenants_discovers_rowless_live_build() {
        let db = TestDb::new(&crate::MIGRATOR).await;
        let tenant = seed_tenant(&db.pool, "rowless").await;

        // Seed the derivation + job WITHOUT the wanted row: a build
        // that is a member only.
        let drv_hash = "rowless-tenant-drv";
        let derivation_id: Uuid = sqlx::query_scalar(
            "INSERT INTO derivations \
                 (drv_hash, drv_path, system, status, output_names, expected_output_paths) \
             VALUES ($1, $2, 'x86_64-linux', 'ready', $3, $4) \
             RETURNING derivation_id",
        )
        .bind(drv_hash)
        .bind(format!("/nix/store/{drv_hash}.drv"))
        .bind(vec!["out".to_string()])
        .bind(vec!["/nix/store/aaa-rowless".to_string()])
        .fetch_one(&db.pool)
        .await
        .expect("derivation seeded");
        let build_id = Uuid::new_v4();
        sqlx::query("INSERT INTO builds (build_id, tenant_id, status) VALUES ($1, $2, 'active')")
            .bind(build_id)
            .bind(tenant)
            .execute(&db.pool)
            .await
            .expect("build seeded");
        sqlx::query("INSERT INTO build_derivations (build_id, derivation_id) VALUES ($1, $2)")
            .bind(build_id)
            .bind(derivation_id)
            .execute(&db.pool)
            .await
            .expect("membership seeded");
        let job_id = Uuid::now_v7();
        sqlx::query(
            "INSERT INTO materialization_jobs \
                 (job_id, derivation_id, drv_hash, tenant_id, origin, created_generation) \
             VALUES ($1, $2, $3, NULL, 'cache_opportunity', 1)",
        )
        .bind(job_id)
        .bind(derivation_id)
        .bind(drv_hash)
        .execute(&db.pool)
        .await
        .expect("job seeded");

        let ctx = make_ctx(db.pool.clone());
        let claimed = ClaimedJob {
            job_id,
            drv_hash: drv_hash.to_string(),
            tenant_hint: None,
            origin: "cache_opportunity".to_string(),
            exec_id: Uuid::now_v7().to_string(),
            drv_path: format!("/nix/store/{drv_hash}.drv"),
        };
        // merged_bug_028: the resolution is PLURAL now — the row-less
        // member's tenant is discoverable AND the deterministic
        // ordering puts it first absent a live hint.
        let resolved = resolve_tenants(&ctx, &claimed).await.expect("query ok");
        assert_eq!(
            resolved,
            vec![tenant],
            "a row-less live member's tenant is discoverable for the walk"
        );

        // The live wanted set saturates to all declared outputs for the
        // row-less member (the '{}' default), not to nothing.
        let paths = live_wanted_paths(&ctx, &claimed).await.expect("query ok");
        assert_eq!(
            paths,
            vec!["/nix/store/aaa-rowless".to_string()],
            "row-less live interest saturates the wanted width to all declared"
        );
    }

    fn outcome_retry_later(
        o: &MaterializationOutcome,
    ) -> Option<&materialization_outcome::RetryLater> {
        match &o.outcome {
            Some(materialization_outcome::Outcome::RetryLater(r)) => Some(r),
            _ => None,
        }
    }

    // r[verify store.materialize.executor+5]
    /// merged_bug_193 (193a): a REFERENCE dep that 404s everywhere
    /// lands in `missing_reference_paths`, NOT `missing_paths` — the
    /// wanted root itself was obtained and rides `verified_paths`.
    /// RED (pre-fix): the field did not exist; the reference miss was
    /// lumped into `missing_paths` and the consumer's moot arm could
    /// complete over the punctured closure.
    #[tokio::test]
    async fn unobtainable_reference_miss_lands_in_reference_cell() {
        let db = TestDb::new(&crate::MIGRATOR).await;
        let tenant = seed_tenant(&db.pool, "mat-refmiss").await;

        let root = store_path(20, "refmiss-root");
        let dep = store_path(21, "refmiss-dep");
        let (root_nar, _) = rio_test_support::fixtures::make_nar(b"refmiss-root");
        // Upstream serves the ROOT (whose narinfo references dep) but
        // 404s the dep itself.
        let upstream = spawn_multi_upstream(
            vec![(root.clone(), root_nar, vec![dep.clone()])],
            "cache.refmiss",
        )
        .await;
        wire_upstream(&db.pool, tenant, &upstream).await;

        let seeded = seed_job(
            &db.pool,
            "mat-refmiss-drv",
            &[("out", root.as_str())],
            Some(tenant),
            Some(tenant),
            &[],
        )
        .await;

        let ctx = make_ctx(db.pool.clone());
        let outcome = execute_job(&ctx, &seeded.claimed, admitted(&ctx))
            .await
            .into_outcome();

        let unobtainable = outcome_unobtainable(&outcome)
            .unwrap_or_else(|| panic!("expected Unobtainable, got {outcome:?}"));
        assert_eq!(
            unobtainable.missing_paths,
            Vec::<String>::new(),
            "no WANTED path is missing — the root was obtained"
        );
        assert_eq!(
            unobtainable.missing_reference_paths,
            vec![dep.clone()],
            "the confirmed-absent reference rides its own cell"
        );
        assert!(
            unobtainable.verified_paths.contains(&root),
            "the obtained root rides verified_paths"
        );
    }

    // r[verify store.materialize.probe-polarity]
    /// bug_295 red: an upstream that 404s narinfo GETs but 429s
    /// narinfo HEADs (method-split rate limiting — real CDNs do this:
    /// the GET lane is cache-fronted, the HEAD probe lane is
    /// origin-billed). The attempt leg cleanly misses (GET 404); the
    /// miss-confirmation HEAD probe is rate-limited with a
    /// `Retry-After` exceeding the probe budget. Pre-fix the terminal
    /// 429 was laundered into `indeterminate` → MissProbe::Infra →
    /// InfraFailure: a PROBE-leg rate-limit wave burned the park
    /// budget the ATTEMPT leg was already shielding (classification
    /// congruence per CLASS, §5-Q23). Post-fix the probe answer rides
    /// the same truth table: 429 → RetryUncharged → RetryLater.
    ///
    /// Recorded red (pre-fix): `expected RetryLater, got
    /// InfraFailure("substitution of … hit infrastructure trouble:
    /// availability probe indeterminate (upstream 5xx/timeout/429)")`.
    #[tokio::test]
    async fn probe_only_rate_limit_defers_uncharged() {
        let db = TestDb::new(&crate::MIGRATOR).await;
        let tenant = seed_tenant(&db.pool, "mat-headlimit").await;

        // Method-split upstream: GET → 404 (clean miss), HEAD → 429
        // with Retry-After far past the probe budget (terminal).
        let app = axum::Router::new().fallback(axum::routing::any(
            move |method: axum::http::Method| async move {
                if method == axum::http::Method::HEAD {
                    let mut h = axum::http::HeaderMap::new();
                    h.insert("Retry-After", axum::http::HeaderValue::from_static("300"));
                    (axum::http::StatusCode::TOO_MANY_REQUESTS, h)
                } else {
                    (
                        axum::http::StatusCode::NOT_FOUND,
                        axum::http::HeaderMap::new(),
                    )
                }
            },
        ));
        let listener =
            tokio::net::TcpListener::bind(std::net::SocketAddr::from(([127, 0, 0, 1], 0)))
                .await
                .unwrap();
        let addr = listener.local_addr().unwrap();
        let task = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let upstream = FakeUpstream {
            url: format!("http://{addr}"),
            trusted_key: "dummy:AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=".into(),
            _task: task,
        };
        wire_upstream(&db.pool, tenant, &upstream).await;

        let path = store_path(25, "mat-headlimit");
        let seeded = seed_job(
            &db.pool,
            "mat-headlimit-drv",
            &[("out", path.as_str())],
            Some(tenant),
            Some(tenant),
            &[],
        )
        .await;

        let ctx = make_ctx(db.pool.clone());
        let outcome = execute_job(&ctx, &seeded.claimed, admitted(&ctx))
            .await
            .into_outcome();
        let retry = outcome_retry_later(&outcome).unwrap_or_else(|| {
            panic!(
                "expected RetryLater (probe-leg 429 closes uncharged), got {outcome:?} — \
                 the HEAD rate-limit was charged as infrastructure"
            )
        });
        assert_eq!(
            retry.class, "rate_limited",
            "the 429 class rides the report"
        );
        assert_eq!(
            retry.retry_after_secs, 300,
            "the upstream's Retry-After advice rides the report"
        );
        // bug_194: the deferral's DETAIL must agree with its class —
        // a rate-limited probe narrated as "infrastructure trouble"
        // is the scheduler logging a contradiction verbatim.
        assert!(
            !retry.detail.contains("infrastructure trouble"),
            "a rate-limited probe deferral must not narrate \
             infrastructure trouble (bug_194); detail={:?}",
            retry.detail
        );
    }

    // r[verify store.materialize.executor+5]
    /// bug_042: paths LOCALLY present (complete manifests) verify and
    /// extend the walk from the LOCAL row's references — upstream
    /// absence is irrelevant. RED (pre-fix): all-404 upstreams turned
    /// a locally-present closure into Unobtainable (the local probe
    /// was only a verified-vs-ingested label, never a verdict input).
    #[tokio::test]
    async fn locally_present_upstream_absent_verifies_and_walks_local_refs() {
        let db = TestDb::new(&crate::MIGRATOR).await;
        let tenant = seed_tenant(&db.pool, "mat-local").await;

        let parent = store_path(22, "local-parent");
        let child = store_path(23, "local-child");
        let (parent_nar, _) = rio_test_support::fixtures::make_nar(b"local-parent");
        let (child_nar, _) = rio_test_support::fixtures::make_nar(b"local-child");

        // Phase 1: ingest both via a temporary upstream (parent
        // references child) so complete LOCAL manifests exist.
        let upstream = spawn_multi_upstream(
            vec![
                (parent.clone(), parent_nar, vec![child.clone()]),
                (child.clone(), child_nar, vec![]),
            ],
            "cache.local-seed",
        )
        .await;
        wire_upstream(&db.pool, tenant, &upstream).await;
        let seeded1 = seed_job(
            &db.pool,
            "mat-local-seed-drv",
            &[("out", parent.as_str())],
            Some(tenant),
            Some(tenant),
            &[],
        )
        .await;
        let ctx = make_ctx(db.pool.clone());
        let first = execute_job(&ctx, &seeded1.claimed, admitted(&ctx))
            .await
            .into_outcome();
        assert!(
            outcome_success(&first).is_some(),
            "phase-1 ingest must succeed, got {first:?}"
        );

        // Phase 2: drop the seed upstream and re-point the SAME tenant
        // at one that 404s everything — the job must verify from the
        // LOCAL rows alone. The rows are tenant-owned, so bug_115's
        // sig-visibility gate passes for the owner; a FOREIGN tenant's
        // view of these rows is pinned HIDDEN (the laundering red) by
        // vis_untrusted cells + tests/substitute_visibility.rs — the
        // pre-gate version of this phase asserted exactly that
        // laundering and was retired with the gate.
        // The trusted KEY must survive: sig-trust against the
        // tenant's configured upstreams is exactly what makes the
        // locally-present rows visible (deleting the row would
        // gate-hide the tenant's own ingest and re-route the walk
        // through the substitute cache). Only the URL goes dead.
        let dead = spawn_status_upstream(axum::http::StatusCode::NOT_FOUND).await;
        sqlx::query("UPDATE tenant_upstreams SET url = $2 WHERE tenant_id = $1")
            .bind(tenant)
            .bind(&dead.url)
            .execute(&db.pool)
            .await
            .expect("seed upstream re-pointed at the dead server");

        let seeded2 = seed_job(
            &db.pool,
            "mat-local-verify-drv",
            &[("out", parent.as_str())],
            Some(tenant),
            Some(tenant),
            &[],
        )
        .await;
        let outcome = execute_job(&ctx, &seeded2.claimed, admitted(&ctx))
            .await
            .into_outcome();
        let success = outcome_success(&outcome)
            .unwrap_or_else(|| panic!("expected Success from local presence, got {outcome:?}"));
        assert!(
            success.verified_paths.contains(&parent) && success.verified_paths.contains(&child),
            "both locally-present paths verify (parent walked to child via the \
             LOCAL row's references): {success:?}"
        );
        assert!(success.ingested_paths.is_empty(), "nothing was fetched");
        assert_eq!(
            pin_count(&db.pool, "mat-local-verify-drv", "materialization").await,
            2,
            "both verified paths are pinned"
        );
    }

    /// bug_042 (error leg): a failing local-presence probe is
    /// INFRASTRUCTURE trouble, never evidence of absence. RED
    /// (pre-fix): `.ok().flatten()` mapped the probe error to
    /// "absent" and the walk pressed on toward an absence verdict.
    #[tokio::test]
    async fn local_probe_error_is_infra_failure() {
        let db = TestDb::new(&crate::MIGRATOR).await;
        let tenant = seed_tenant(&db.pool, "mat-probe-err").await;
        let path = store_path(24, "probe-err");
        let (nar, _) = rio_test_support::fixtures::make_nar(b"probe-err");
        let upstream =
            spawn_multi_upstream(vec![(path.clone(), nar, vec![])], "cache.probe-err").await;
        wire_upstream(&db.pool, tenant, &upstream).await;
        let seeded = seed_job(
            &db.pool,
            "mat-probe-err-drv",
            &[("out", path.as_str())],
            Some(tenant),
            Some(tenant),
            &[],
        )
        .await;
        // Break the local-probe surface specifically: narinfo is read
        // by `query_path_info` but by neither the wanted-set join nor
        // the substituter's claim path... it IS read by ingest.
        // Renaming the table makes the probe the first failure point.
        sqlx::query("ALTER TABLE narinfo RENAME TO narinfo_broken")
            .execute(&db.pool)
            .await
            .unwrap();
        let ctx = make_ctx(db.pool.clone());
        let outcome = execute_job(&ctx, &seeded.claimed, admitted(&ctx))
            .await
            .into_outcome();
        let infra = outcome_infra(&outcome)
            .unwrap_or_else(|| panic!("expected InfraFailure, got {outcome:?}"));
        assert!(
            infra.detail.contains("local presence probe failed"),
            "the probe error names itself: {}",
            infra.detail
        );
    }

    // r[verify store.materialize.executor+5]
    /// merged_bug_178 (178a): an upstream 429 with Retry-After is a
    /// transient, UNCHARGED RetryLater — class and parsed delay ride
    /// the outcome. RED (pre-fix): `Err(RateLimited)` fell into the
    /// blanket infra arm and charged the materialization budget.
    #[tokio::test]
    async fn rate_limited_reports_retry_later_with_retry_after() {
        use axum::{Router, routing::get};
        let db = TestDb::new(&crate::MIGRATOR).await;
        let tenant = seed_tenant(&db.pool, "mat-429").await;
        let path = store_path(25, "ratelimited");

        // An upstream that 429s every narinfo GET with Retry-After: 42.
        let app = Router::new()
            .route(
                "/nix-cache-info",
                get(|| async { "StoreDir: /nix/store\nWantMassQuery: 1\nPriority: 40\n" }),
            )
            .fallback(get(|| async {
                (
                    axum::http::StatusCode::TOO_MANY_REQUESTS,
                    [(axum::http::header::RETRY_AFTER, "42")],
                    "slow down",
                )
            }));
        let listener = tokio::net::TcpListener::bind(SocketAddr::from(([127, 0, 0, 1], 0)))
            .await
            .unwrap();
        let addr = listener.local_addr().unwrap();
        let _task = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        sqlx::query(
            "INSERT INTO tenant_upstreams (tenant_id, url, priority, trusted_keys, sig_mode) \
             VALUES ($1, $2, 50, $3, 'keep')",
        )
        .bind(tenant)
        .bind(format!("http://{addr}"))
        .bind(vec!["cache.429:AAAA".to_string()])
        .execute(&db.pool)
        .await
        .unwrap();

        let seeded = seed_job(
            &db.pool,
            "mat-429-drv",
            &[("out", path.as_str())],
            Some(tenant),
            Some(tenant),
            &[],
        )
        .await;
        let ctx = make_ctx(db.pool.clone());
        let outcome = execute_job(&ctx, &seeded.claimed, admitted(&ctx))
            .await
            .into_outcome();
        let retry = outcome_retry_later(&outcome)
            .unwrap_or_else(|| panic!("expected RetryLater, got {outcome:?}"));
        assert_eq!(retry.class, "rate_limited");
        assert_eq!(
            retry.retry_after_secs, 42,
            "parsed Retry-After rides the outcome"
        );
    }

    // r[verify store.materialize.executor+5]
    /// merged_bug_178 (178a): a placeholder race (another uploader
    /// holds the slot) is RetryLater too — the in-flight upload will
    /// land; charging would burn budget on our own concurrency. RED
    /// (pre-fix): `Err(Raced)` charged infra.
    #[tokio::test]
    async fn raced_placeholder_reports_retry_later() {
        let db = TestDb::new(&crate::MIGRATOR).await;
        let tenant = seed_tenant(&db.pool, "mat-raced").await;
        let path = store_path(26, "raced");
        let (nar, _) = rio_test_support::fixtures::make_nar(b"raced");
        let upstream = spawn_multi_upstream(vec![(path.clone(), nar, vec![])], "cache.raced").await;
        wire_upstream(&db.pool, tenant, &upstream).await;

        // A YOUNG 'uploading' placeholder held by a concurrent
        // uploader → claim_placeholder answers Concurrent → Raced.
        let sp = StorePath::parse(&path).unwrap();
        let hash = sp.sha256_digest();
        crate::metadata::insert_manifest_uploading(&db.pool, &hash, &path, &[])
            .await
            .unwrap();

        let seeded = seed_job(
            &db.pool,
            "mat-raced-drv",
            &[("out", path.as_str())],
            Some(tenant),
            Some(tenant),
            &[],
        )
        .await;
        let ctx = make_ctx(db.pool.clone());
        let outcome = execute_job(&ctx, &seeded.claimed, admitted(&ctx))
            .await
            .into_outcome();
        let retry = outcome_retry_later(&outcome)
            .unwrap_or_else(|| panic!("expected RetryLater, got {outcome:?}"));
        assert_eq!(retry.class, "raced");
    }

    /// GREEN PIN (merged_bug_178 scope narrowing): a stalled download
    /// STAYS InfraFailure — `stalled_download_reports_infra_failure`
    /// above is the pin; this test documents the table row.
    #[test]
    fn stalled_and_admission_stay_charging() {
        use rio_evidence_kernel::outcome::*;
        assert_eq!(
            classify_substitute_failure(SubstituteFailureClass::Stalled),
            FailureDisposition::ChargeInfra
        );
        assert_eq!(
            classify_substitute_failure(SubstituteFailureClass::AdmissionSaturated),
            FailureDisposition::ChargeInfra
        );
    }

    // r[verify store.materialize.executor+5]
    /// merged_bug_194 (store leg): a FIRST iteration that resolves no
    /// verifiable wanted path reports infra — never "Success with
    /// nothing verified". RED (pre-fix): the empty-seed break produced
    /// a vacuous Success.
    #[tokio::test]
    async fn first_iteration_empty_wanted_reports_infra_not_success() {
        let db = TestDb::new(&crate::MIGRATOR).await;
        let tenant = seed_tenant(&db.pool, "mat-vacuous").await;
        // Floating-CA shape: the expected output path is the [""]
        // placeholder, no carrier on the job → zero verifiable paths.
        let seeded = seed_job(
            &db.pool,
            "mat-vacuous-drv",
            &[("out", "")],
            Some(tenant),
            Some(tenant),
            &[],
        )
        .await;
        let ctx = make_ctx(db.pool.clone());
        let outcome = execute_job(&ctx, &seeded.claimed, admitted(&ctx))
            .await
            .into_outcome();
        let infra = outcome_infra(&outcome)
            .unwrap_or_else(|| panic!("expected InfraFailure, got {outcome:?}"));
        assert!(
            infra.detail.contains("no-verifiable-wanted-paths"),
            "the vacuous shape names itself: {}",
            infra.detail
        );
    }

    // r[verify store.materialize.executor+5]
    /// merged_bug_028 / owner Q2 (executor leg): when the FIRST
    /// tenant's upstreams 404 a path, the walk tries the NEXT
    /// interested tenant's upstreams and succeeds — the job fails only
    /// when NO interested tenant can obtain. RED (pre-fix): the
    /// singular resolve_tenant honored the hint tenant exclusively;
    /// tenant1's 404 became Unobtainable with tenant2's serving
    /// upstream never consulted.
    #[tokio::test]
    async fn second_tenant_upstream_serves_path() {
        let db = TestDb::new(&crate::MIGRATOR).await;
        let tenant1 = seed_tenant(&db.pool, "mat-mt-1").await;
        let tenant2 = seed_tenant(&db.pool, "mat-mt-2").await;

        let path = store_path(27, "multi-tenant");
        let (nar, _) = rio_test_support::fixtures::make_nar(b"multi-tenant");
        // tenant1's only upstream 404s everything; tenant2's serves it.
        let dead = spawn_status_upstream(axum::http::StatusCode::NOT_FOUND).await;
        wire_upstream(&db.pool, tenant1, &dead).await;
        let live = spawn_multi_upstream(vec![(path.clone(), nar, vec![])], "cache.mt").await;
        wire_upstream(&db.pool, tenant2, &live).await;

        // Both tenants interested: the job's hint is tenant1 (the
        // 404-only one — deterministic pre-fix red), and a SECOND live
        // build under tenant2 also wants the node.
        let seeded = seed_job(
            &db.pool,
            "mat-mt-drv",
            &[("out", path.as_str())],
            Some(tenant1),
            Some(tenant1),
            &[],
        )
        .await;
        let build2 = Uuid::new_v4();
        sqlx::query("INSERT INTO builds (build_id, tenant_id, status) VALUES ($1, $2, 'active')")
            .bind(build2)
            .bind(tenant2)
            .execute(&db.pool)
            .await
            .unwrap();
        sqlx::query("INSERT INTO build_derivations (build_id, derivation_id) VALUES ($1, $2)")
            .bind(build2)
            .bind(seeded.derivation_id)
            .execute(&db.pool)
            .await
            .unwrap();
        sqlx::query(
            "INSERT INTO build_wanted_outputs (build_id, derivation_id, wanted_output_names) \
             VALUES ($1, $2, '{}')",
        )
        .bind(build2)
        .bind(seeded.derivation_id)
        .execute(&db.pool)
        .await
        .unwrap();

        let ctx = make_ctx(db.pool.clone());
        let outcome = execute_job(&ctx, &seeded.claimed, admitted(&ctx))
            .await
            .into_outcome();
        let success = outcome_success(&outcome)
            .unwrap_or_else(|| panic!("expected Success via tenant2, got {outcome:?}"));
        assert_eq!(
            success.ingested_paths,
            vec![path.clone()],
            "tenant2's upstream served the path after tenant1 missed"
        );
    }

    // r[verify store.materialize.executor+5]
    /// merged_bug_028 (conjunction leg): a confirmed-absent verdict
    /// requires the miss to be confirmed under EVERY interested
    /// tenant — when one tenant's probe is indeterminate (its upstream
    /// 5xxes), the walk reports infrastructure trouble, never
    /// Unobtainable.
    #[tokio::test]
    async fn miss_with_indeterminate_second_tenant_is_infra() {
        let db = TestDb::new(&crate::MIGRATOR).await;
        let tenant1 = seed_tenant(&db.pool, "mat-mtind-1").await;
        let tenant2 = seed_tenant(&db.pool, "mat-mtind-2").await;

        let path = store_path(28, "mt-indeterminate");
        // tenant1: clean 404; tenant2: 500 (indeterminate probe).
        let dead = spawn_status_upstream(axum::http::StatusCode::NOT_FOUND).await;
        wire_upstream(&db.pool, tenant1, &dead).await;
        let broken = spawn_status_upstream(axum::http::StatusCode::INTERNAL_SERVER_ERROR).await;
        wire_upstream(&db.pool, tenant2, &broken).await;

        let seeded = seed_job(
            &db.pool,
            "mat-mtind-drv",
            &[("out", path.as_str())],
            Some(tenant1),
            Some(tenant1),
            &[],
        )
        .await;
        let build2 = Uuid::new_v4();
        sqlx::query("INSERT INTO builds (build_id, tenant_id, status) VALUES ($1, $2, 'active')")
            .bind(build2)
            .bind(tenant2)
            .execute(&db.pool)
            .await
            .unwrap();
        sqlx::query("INSERT INTO build_derivations (build_id, derivation_id) VALUES ($1, $2)")
            .bind(build2)
            .bind(seeded.derivation_id)
            .execute(&db.pool)
            .await
            .unwrap();
        sqlx::query(
            "INSERT INTO build_wanted_outputs (build_id, derivation_id, wanted_output_names) \
             VALUES ($1, $2, '{}')",
        )
        .bind(build2)
        .bind(seeded.derivation_id)
        .execute(&db.pool)
        .await
        .unwrap();

        let ctx = make_ctx(db.pool.clone());
        let outcome = execute_job(&ctx, &seeded.claimed, admitted(&ctx))
            .await
            .into_outcome();
        assert!(
            outcome_infra(&outcome).is_some(),
            "an unconfirmable tenant view must report infra, got {outcome:?}"
        );
    }

    // ── bug_159: monotone progress ────────────────────────────────────

    // r[verify store.materialize.progress-monotone+1]
    /// bug_159: the stall-failover regression trace. Path A completes
    /// at 100 cumulative; path B (base 100) streams to 120 relative,
    /// the download stalls, and the failover attempt restarts the
    /// per-fetch counter at 0. bug_159's original defect: the raw
    /// forward regressed below the COMMITTED 100. bug_087 narrows the
    /// guarantee to exactly that floor: provisional emissions clamp
    /// at committed work (110 = 100 + 10 streamed is truthful display
    /// after the reset — it may step back from a dead attempt's
    /// provisional peak, which was never committed), and only
    /// `commit` raises the floor.
    #[test]
    fn monotone_progress_clamps_stall_failover_resets() {
        use std::sync::{Arc, Mutex};
        let got: Arc<Mutex<Vec<(u64, u64)>>> = Arc::default();
        let sink = Arc::clone(&got);
        let progress = MonotoneProgress::new(move |d: u64, e: u64, _u: &str| {
            sink.lock().unwrap().push((d, e));
        });

        progress.commit(100, ""); // path A fully processed
        // Path B's ticks arrive base-adjusted (base = 100) as EVENTS
        // on the driver's stream (path-fold law 6); the driver is the
        // sole caller, forwarding them here as provisional emissions.
        progress.emit_provisional(100 + 120, 100 + 200, "u1"); // attempt 1 streams 120 of 200
        progress.emit_provisional(100 + 10, 100 + 200, "u2"); // stall failover: counter RESET to 10
        progress.emit_provisional(100 + 180, 100 + 200, "u2"); // attempt 2 catches up past the mark
        progress.commit(300, ""); // path B fully processed

        let events = got.lock().unwrap().clone();
        assert_eq!(
            events,
            vec![(100, 100), (220, 300), (110, 300), (280, 300), (300, 300)],
            "provisional resets clamp at the COMMITTED floor (truthful \
             display), never below it; commits raise the floor"
        );
        let floor_then = [0u64, 100, 100, 100, 100];
        for ((d, e), floor) in events.iter().zip(floor_then) {
            assert!(
                *d >= floor,
                "emitted done {d} below committed floor {floor}"
            );
            assert!(d <= e, "done {d} > expected {e}");
        }
        assert_eq!(
            *got.lock().unwrap().last().unwrap(),
            (300, 300),
            "the final tick is the true cumulative total"
        );
    }

    /// bug_087 RED-FIRST: bytes streamed by a FAILED attempt must not
    /// floor the job. A 5 GB partial stream that dies mid-fetch, then
    /// a smaller successful path: the final report must equal the
    /// TRUE total (the success), not the dead attempt's peak — the
    /// pre-fix adapter fetch_max'ed every provisional candidate into
    /// the job-wide high-water, so clamp dragged `expected` up to the
    /// inflated `done` and the final BC-4 report showed
    /// done == expected ABOVE the closure's true byte total.
    // r[verify store.materialize.progress-monotone+1]
    #[test]
    fn failed_attempt_bytes_never_floor_the_job() {
        let emitted: std::sync::Arc<std::sync::Mutex<Vec<(u64, u64)>>> =
            std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let sink = std::sync::Arc::clone(&emitted);
        let progress = MonotoneProgress::new(move |d, e, _u: &str| {
            sink.lock().unwrap().push((d, e));
        });

        // Attempt 1: a large partial stream's ticks forwarded by the
        // driver (base = 0)... then the fetch FAILS (no success
        // commit).
        progress.emit_provisional(1_000_000, 5_000_000_000, "https://big");
        progress.emit_provisional(4_999_999_999, 5_000_000_000, "https://big");

        // Attempt 2 (a different, smaller path) succeeds: the path's
        // bytes are committed into the job floor.
        progress.commit(1_200, "");

        let last = *emitted.lock().unwrap().last().unwrap();
        assert_eq!(
            last,
            (1_200, 1_200),
            "the final tick is the TRUE total — a failed attempt's \
             provisional bytes must not survive as the job floor"
        );
    }

    proptest::proptest! {
        // r[verify store.materialize.progress-monotone+1]
        /// The pure clamp law over ARBITRARY interleavings of
        /// provisional candidates and floor commits (bug_087): every
        /// emission is >= the committed floor at that moment, done <=
        /// expected at every step, and the floor itself never
        /// regresses.
        #[test]
        fn clamp_progress_respects_committed_floor_over_arbitrary_traces(
            events in proptest::collection::vec(
                (proptest::bool::ANY, 0u64..1_000_000, 0u64..1_000_000),
                1..100
            )
        ) {
            let mut floor = 0u64;
            for (is_commit, done, expected) in events {
                if is_commit {
                    let prev = floor;
                    floor = floor.max(done);
                    let (d, e) = clamp_progress(prev, done, done);
                    proptest::prop_assert!(d >= prev && d <= e);
                    proptest::prop_assert!(floor >= prev, "floor regressed");
                } else {
                    let (d, e) = clamp_progress(floor, done, expected);
                    proptest::prop_assert!(d >= floor, "emitted below floor: {} < {}", d, floor);
                    proptest::prop_assert!(d <= e, "done {} > expected {}", d, e);
                }
            }
        }
    }

    // ── bug_244: outcome-label alphabet drift gate ────────────────────

    /// bug_244: the three tiers (seed array, emit match, HELP) all
    /// derive from OUTCOME_LABELS, and this gate pins the const to the
    /// image of `outcome_label` over the full oneof alphabet + None.
    /// A sixth outcome that misses any tier fails to compile
    /// (non-exhaustive match) or fails here. RED (pre-fix, captured by
    /// pointing the const at the old 4-label seed array): missing
    /// `retry_later`; and the HELP helper red on the stale 3-label
    /// text.
    #[test]
    fn outcome_label_alphabet_single_source() {
        use materialization_outcome::*;
        let image = [
            outcome_label(Some(&Outcome::Success(Success::default()))),
            outcome_label(Some(&Outcome::Unobtainable(Unobtainable::default()))),
            outcome_label(Some(&Outcome::InfraFailure(InfraFailure::default()))),
            outcome_label(Some(&Outcome::Aborted(Aborted::default()))),
            outcome_label(Some(&Outcome::RetryLater(RetryLater::default()))),
            outcome_label(None),
        ];
        // Every label producible by the match is in the const…
        for label in image {
            assert!(
                OUTCOME_LABELS.contains(&label),
                "emit produces {label:?} but OUTCOME_LABELS does not seed it"
            );
        }
        // …every const label is producible (no dead seeds)…
        for label in OUTCOME_LABELS {
            assert!(
                image.contains(&label),
                "OUTCOME_LABELS seeds {label:?} but outcome_label never emits it"
            );
        }
        // …no duplicate seeds…
        let mut dedup = OUTCOME_LABELS.to_vec();
        dedup.sort_unstable();
        dedup.dedup();
        assert_eq!(
            dedup.len(),
            OUTCOME_LABELS.len(),
            "duplicate label in const"
        );
        // …and the operator-facing HELP (the literal at the
        // describe_counter! site — the docs-data scraper requires a
        // literal there, so it cannot interpolate the const) names
        // every label. Captured through a local recorder so the gate
        // reads the EXACT text the scraper and Prometheus see.
        #[derive(Default)]
        struct HelpCapture(std::sync::Mutex<std::collections::HashMap<String, String>>);
        impl metrics::Recorder for HelpCapture {
            fn describe_counter(
                &self,
                key: metrics::KeyName,
                _u: Option<metrics::Unit>,
                help: metrics::SharedString,
            ) {
                self.0
                    .lock()
                    .unwrap()
                    .insert(key.as_str().to_string(), help.into_owned());
            }
            fn describe_gauge(
                &self,
                _: metrics::KeyName,
                _: Option<metrics::Unit>,
                _: metrics::SharedString,
            ) {
            }
            fn describe_histogram(
                &self,
                _: metrics::KeyName,
                _: Option<metrics::Unit>,
                _: metrics::SharedString,
            ) {
            }
            fn register_counter(
                &self,
                _: &metrics::Key,
                _: &metrics::Metadata<'_>,
            ) -> metrics::Counter {
                metrics::Counter::noop()
            }
            fn register_gauge(
                &self,
                _: &metrics::Key,
                _: &metrics::Metadata<'_>,
            ) -> metrics::Gauge {
                metrics::Gauge::noop()
            }
            fn register_histogram(
                &self,
                _: &metrics::Key,
                _: &metrics::Metadata<'_>,
            ) -> metrics::Histogram {
                metrics::Histogram::noop()
            }
        }
        let recorder = HelpCapture::default();
        metrics::with_local_recorder(&recorder, crate::describe_metrics);
        let help = recorder
            .0
            .lock()
            .unwrap()
            .get("rio_store_materialization_executions_total")
            .cloned()
            .expect("executions counter must be described");
        for label in OUTCOME_LABELS {
            assert!(
                help.contains(label),
                "HELP must name {label:?}; alphabet text drifted: {help}"
            );
        }
    }

    // ── bug_115: local-presence visibility ────────────────────────────

    /// Seed a COMPLETE local manifest for `path` carrying exactly
    /// `signatures` (the sign.rs-test idiom: placeholder claim +
    /// inline completion).
    async fn seed_local_manifest(pool: &PgPool, path: &str, signatures: Vec<String>) {
        let (nar, nar_hash) = rio_test_support::fixtures::make_nar(path.as_bytes());
        let mut info = rio_test_support::fixtures::make_path_info(path, &nar, nar_hash);
        let sp = StorePath::parse(path).unwrap();
        let hash = sp.sha256_digest();
        info.store_path_hash = hash.to_vec();
        info.signatures = signatures;
        let claim = metadata::insert_manifest_uploading(pool, &hash, path, &[])
            .await
            .unwrap()
            .unwrap();
        metadata::complete_manifest_inline(pool, &info, claim, nar.into())
            .await
            .unwrap();
    }

    // r[verify store.materialize.local-visibility]
    /// bug_115 cell 1 (substitution-only, untrusted sig): a local row
    /// signed ONLY by a key no interested tenant trusts must NOT be
    /// served/pinned/verified by the walk — it degrades to the
    /// per-tenant substitute lane, and with every upstream 404ing the
    /// job is Unobtainable. RED (pre-fix): raw physical presence was
    /// sufficient — Success with the hidden row in verified_paths,
    /// laundered into per-tenant ownership at consumption
    /// (upsert_path_tenants_for_batch stamps every interested build's
    /// tenant over the job's verified paths).
    #[tokio::test]
    async fn local_row_with_untrusted_sig_is_not_laundered() {
        let db = TestDb::new(&crate::MIGRATOR).await;
        let tenant = seed_tenant(&db.pool, "vis-untrusted").await;
        // The tenant's upstream trusts an unrelated key and serves
        // NOTHING (404): trust context exists; the path is simply not
        // obtainable under this tenant's view.
        let upstream = spawn_status_upstream(axum::http::StatusCode::NOT_FOUND).await;
        wire_upstream(&db.pool, tenant, &upstream).await;

        // Local complete manifest signed ONLY by key K (untrusted).
        let path = store_path(40, "vis-untrusted");
        let signer_k = Signer::from_seed("key-K", &[0x7Au8; 32]);
        let (nar, nar_hash) = rio_test_support::fixtures::make_nar(path.as_bytes());
        let fp = fingerprint(&path, &nar_hash, nar.len() as u64, &[]);
        seed_local_manifest(&db.pool, &path, vec![signer_k.sign(&fp)]).await;

        let seeded = seed_job(
            &db.pool,
            "vis-untrusted-drv",
            &[("out", path.as_str())],
            Some(tenant),
            Some(tenant),
            &[],
        )
        .await;
        let ctx = make_ctx(db.pool.clone());
        let outcome = execute_job(&ctx, &seeded.claimed, admitted(&ctx))
            .await
            .into_outcome();
        let unobtainable = outcome_unobtainable(&outcome).unwrap_or_else(|| {
            panic!("expected Unobtainable (gate-hidden local row must degrade), got {outcome:?}")
        });
        assert_eq!(
            unobtainable.missing_paths,
            vec![path.clone()],
            "the hidden row is missing-wanted under this tenant's view"
        );
        // Nothing pinned: the hidden row was never treated as served.
        assert_eq!(
            pin_count(&db.pool, "vis-untrusted-drv", "materialization").await,
            0,
            "a gate-hidden local row must not be pinned"
        );
    }

    // r[verify store.materialize.local-visibility]
    /// bug_115 cell 2 (I-217, built by another tenant only): a local
    /// row OWNED by tenant B must be invisible to interested tenant
    /// A's walk regardless of signatures — built-by-another beats
    /// sig-trust. RED (pre-fix): Success — B's output laundered into
    /// A's job.
    #[tokio::test]
    async fn local_row_built_by_other_tenant_is_hidden_from_walk() {
        let db = TestDb::new(&crate::MIGRATOR).await;
        let tenant_a = seed_tenant(&db.pool, "vis-i217-a").await;
        let tenant_b = seed_tenant(&db.pool, "vis-i217-b").await;
        let upstream = spawn_status_upstream(axum::http::StatusCode::NOT_FOUND).await;
        wire_upstream(&db.pool, tenant_a, &upstream).await;

        let path = store_path(41, "vis-i217");
        seed_local_manifest(&db.pool, &path, vec![]).await;
        // B built it: a path_tenants row for B only.
        let sp = StorePath::parse(&path).unwrap();
        sqlx::query("INSERT INTO path_tenants (store_path_hash, tenant_id) VALUES ($1, $2)")
            .bind(sp.sha256_digest().as_slice())
            .bind(tenant_b)
            .execute(&db.pool)
            .await
            .unwrap();

        let seeded = seed_job(
            &db.pool,
            "vis-i217-drv",
            &[("out", path.as_str())],
            Some(tenant_a),
            Some(tenant_a),
            &[],
        )
        .await;
        let ctx = make_ctx(db.pool.clone());
        let outcome = execute_job(&ctx, &seeded.claimed, admitted(&ctx))
            .await
            .into_outcome();
        assert!(
            outcome_unobtainable(&outcome).is_some(),
            "I-217: another tenant's built output must be hidden from the walk, got {outcome:?}"
        );
    }

    // r[verify store.materialize.local-visibility]
    /// bug_115 positive control: a local row the interested tenant CAN
    /// see (signed by a key the tenant's upstream config trusts) is
    /// served locally — Success with the path verified + pinned, no
    /// upstream byte fetched (the only wired upstream 404s everything).
    #[tokio::test]
    async fn local_row_visible_to_interested_tenant_serves_locally() {
        let db = TestDb::new(&crate::MIGRATOR).await;
        let tenant = seed_tenant(&db.pool, "vis-trusted").await;
        let signer_k = Signer::from_seed("key-K", &[0x7Bu8; 32]);
        // Upstream serves nothing but carries K in trusted_keys: the
        // tenant trusts K without being able to fetch anything.
        let upstream = spawn_status_upstream(axum::http::StatusCode::NOT_FOUND).await;
        metadata::upstreams::insert(
            &db.pool,
            tenant,
            &upstream.url,
            50,
            &[signer_k.trusted_key_entry()],
            SigMode::Keep,
        )
        .await
        .unwrap();

        let path = store_path(42, "vis-trusted");
        let (nar, nar_hash) = rio_test_support::fixtures::make_nar(path.as_bytes());
        let fp = fingerprint(&path, &nar_hash, nar.len() as u64, &[]);
        seed_local_manifest(&db.pool, &path, vec![signer_k.sign(&fp)]).await;

        let seeded = seed_job(
            &db.pool,
            "vis-trusted-drv",
            &[("out", path.as_str())],
            Some(tenant),
            Some(tenant),
            &[],
        )
        .await;
        let ctx = make_ctx(db.pool.clone());
        let outcome = execute_job(&ctx, &seeded.claimed, admitted(&ctx))
            .await
            .into_outcome();
        let success = outcome_success(&outcome)
            .unwrap_or_else(|| panic!("expected Success via the local row, got {outcome:?}"));
        assert_eq!(success.verified_paths, vec![path.clone()]);
        assert!(success.ingested_paths.is_empty(), "no upstream fetch");
        assert_eq!(
            pin_count(&db.pool, "vis-trusted-drv", "materialization").await,
            1,
            "locally-served path is pinned at ingest"
        );
    }

    // r[verify store.materialize.local-visibility]
    /// R1-073 (bug_073, TRUE RED pre-fix): F probes of one job EXECUTE
    /// CONCURRENTLY — W-073b, the strongest behavioral form of "the
    /// probe phase parallelizes", independent of wall-clock. Four
    /// locally-present substitution-only paths (trusted sig — the
    /// cache-consulting arm), F = 4, probe rendezvous barrier sized 4:
    /// the walk completes only if all four probes overlap in time.
    ///
    /// Pre-fix red: rendezvous never completes — siblings are excluded
    /// by the job-wide trust-cache mutex while the first prober parks
    /// inside it (probe phase serialized to width 1; the watchdog
    /// fires). Post-fix: all four probes rendezvous; the walk
    /// completes with all paths Served. (Watchdog 10 s — slack over
    /// the book's 5 s for builder variance; the gate is structural:
    /// pre-fix the rendezvous NEVER completes, so widening cannot
    /// mask the red.)
    #[tokio::test]
    async fn probe_phase_admits_concurrent_paths() {
        let db = TestDb::new(&crate::MIGRATOR).await;
        let tenant = seed_tenant(&db.pool, "probe-conc").await;
        let signer_k = Signer::from_seed("key-K", &[0x7Cu8; 32]);
        // Upstream serves nothing but carries K in trusted_keys: the
        // local rows are substitution-only and VISIBLE via the sig
        // cell (the trusted-set-cache-consulting probe arm).
        let upstream = spawn_status_upstream(axum::http::StatusCode::NOT_FOUND).await;
        metadata::upstreams::insert(
            &db.pool,
            tenant,
            &upstream.url,
            50,
            &[signer_k.trusted_key_entry()],
            SigMode::Keep,
        )
        .await
        .unwrap();

        let paths: Vec<String> = (0..4).map(|i| store_path(60 + i, "probe-conc")).collect();
        for path in &paths {
            let (nar, nar_hash) = rio_test_support::fixtures::make_nar(path.as_bytes());
            let fp = fingerprint(path, &nar_hash, nar.len() as u64, &[]);
            seed_local_manifest(&db.pool, path, vec![signer_k.sign(&fp)]).await;
        }
        let outputs: Vec<(String, &str)> = paths
            .iter()
            .enumerate()
            .map(|(i, p)| (format!("o{i}"), p.as_str()))
            .collect();
        let outputs_ref: Vec<(&str, &str)> =
            outputs.iter().map(|(n, p)| (n.as_str(), *p)).collect();
        let seeded = seed_job(
            &db.pool,
            "probe-conc-drv",
            &outputs_ref,
            Some(tenant),
            Some(tenant),
            &[],
        )
        .await;

        let mut ctx = make_ctx(db.pool.clone()); // F = 4
        ctx.probe_rendezvous = Some(std::sync::Arc::new(tokio::sync::Barrier::new(4)));
        let job = tokio::spawn({
            let claimed = seeded.claimed.clone();
            async move {
                execute_job(&ctx, &claimed, admitted(&ctx))
                    .await
                    .into_outcome()
            }
        });
        let outcome = tokio::time::timeout(Duration::from_secs(10), job)
            .await
            .expect(
                "rendezvous never completes — siblings excluded by the job-wide \
                 trust-cache mutex while the first prober parks inside it \
                 (probe phase serialized to width 1)",
            )
            .unwrap();
        let success = outcome_success(&outcome)
            .unwrap_or_else(|| panic!("expected Success via the local rows, got {outcome:?}"));
        assert_eq!(
            success.verified_paths.len(),
            4,
            "all four probes rendezvoused and every path was served locally"
        );
    }

    /// bug_084: the typed refusal verdict rides the wire on a PURE
    /// content mismatch — `refusal == CONTENT` with the field-5 echo
    /// `false`. Pre-fix this shape was INEXPRESSIBLE (no proto field;
    /// the mismatch reached the scheduler only as cause prose folded
    /// into `error_msg`), so the wire red is compile-level — the
    /// merged_bug_263 precedent recorded at routing.rs. The cells are
    /// the production walk cell type recorded through the production
    /// `record` API; `refusal_wire` IS the production mint (the sole
    /// writer of both fields at the Unobtainable literal).
    #[test]
    fn unobtainable_refusal_field_rides_content_mismatch() {
        use rio_proto::types::UnobtainableRefusal;
        let trust = rio_evidence_kernel::outcome::GenStampedCells::new();
        let mut content = rio_evidence_kernel::outcome::GenStampedCells::new();
        content.record("/nix/store/abc-x".into(), 0);
        let (refusal, echo) = refusal_wire(&trust, &content);
        assert_eq!(
            refusal,
            UnobtainableRefusal::Content as i32,
            "a content disagreement must ride typed, not as cause prose"
        );
        assert!(!echo, "the trust echo is false for a pure content mismatch");
    }

    /// bug_084 coherence table: the ONE constructor derives BOTH wire
    /// fields, so every (trust, content) cell maps to exactly one
    /// coherent pair — the field-5 echo is true iff the refusal value
    /// carries the trust axis (the incoherent shapes the walk cannot
    /// observe are unwritable through this mint).
    #[test]
    fn refusal_wire_pair_is_coherent_across_all_cells() {
        use rio_proto::types::UnobtainableRefusal as R;
        let cell = |on: bool| {
            let mut c = rio_evidence_kernel::outcome::GenStampedCells::new();
            if on {
                c.record("/nix/store/abc-x".into(), 0);
            }
            c
        };
        for (trust_on, content_on, want, want_echo) in [
            (false, false, R::Unspecified, false),
            (true, false, R::Trust, true),
            (false, true, R::Content, false),
            (true, true, R::TrustAndContent, true),
        ] {
            let (refusal, echo) = refusal_wire(&cell(trust_on), &cell(content_on));
            assert_eq!(refusal, want as i32, "cell ({trust_on}, {content_on})");
            assert_eq!(echo, want_echo, "echo at cell ({trust_on}, {content_on})");
        }
    }

    // ── live_047/R-C WO-R7-2B: the path-fold law witnesses (F > 1) ────

    /// Per-path-gated multi-path upstream: like [`spawn_gated_upstream`]
    /// but each path's NAR route awaits its OWN gate (one `notify_one`
    /// per request), so a test controls the COMPLETION ORDER of
    /// concurrent in-flight paths exactly.
    async fn spawn_pathgated_upstream(
        paths: Vec<(String, Vec<u8>, Vec<String>)>,
        key_name: &str,
    ) -> (
        FakeUpstream,
        std::collections::HashMap<String, std::sync::Arc<tokio::sync::Notify>>,
    ) {
        use axum::{Router, routing::get};
        use base64::Engine;
        use sha2::Digest;

        let seed = [0x44u8; 32];
        let signer = Signer::from_seed(key_name, &seed);
        let pubkey = ed25519_dalek::SigningKey::from_bytes(&seed).verifying_key();
        let trusted_key = format!(
            "{key_name}:{}",
            base64::engine::general_purpose::STANDARD.encode(pubkey.as_bytes())
        );
        let mut gates: std::collections::HashMap<String, std::sync::Arc<tokio::sync::Notify>> =
            std::collections::HashMap::new();

        let mut app = Router::new().route(
            "/nix-cache-info",
            get(|| async { "StoreDir: /nix/store\nWantMassQuery: 1\nPriority: 40\n" }),
        );
        for (path, nar, refs) in paths {
            let nar_hash: [u8; 32] = sha2::Sha256::digest(&nar).into();
            let nar_hash_str = format!(
                "sha256:{}",
                rio_nix::store_path::nixbase32::encode(&nar_hash)
            );
            let fp = fingerprint(&path, &nar_hash, nar.len() as u64, &refs);
            let sig = signer.sign(&fp);
            let sp = StorePath::parse(&path).unwrap();
            let hash_part = sp.hash_part();
            let ref_basenames: Vec<&str> = refs
                .iter()
                .map(|r| r.strip_prefix("/nix/store/").unwrap_or(r))
                .collect();
            let narinfo = format!(
                "StorePath: {path}\n\
                 URL: nar/{hash_part}.nar\n\
                 Compression: none\n\
                 NarHash: {nar_hash_str}\n\
                 NarSize: {}\n\
                 References: {}\n\
                 Sig: {sig}\n",
                nar.len(),
                ref_basenames.join(" ")
            );
            let gate = std::sync::Arc::new(tokio::sync::Notify::new());
            gates.insert(path.clone(), std::sync::Arc::clone(&gate));
            app = app
                .route(
                    &format!("/{hash_part}.narinfo"),
                    get(move || async move { narinfo }),
                )
                .route(
                    &format!("/nar/{hash_part}.nar"),
                    get(move || async move {
                        gate.notified().await;
                        nar
                    }),
                );
        }

        let listener = tokio::net::TcpListener::bind(SocketAddr::from(([127, 0, 0, 1], 0)))
            .await
            .unwrap();
        let addr = listener.local_addr().unwrap();
        let task = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        (
            FakeUpstream {
                url: format!("http://{addr}"),
                trusted_key,
                _task: task,
            },
            gates,
        )
    }

    /// W-1c/W-6a fixture: ONE upstream serving path A (narinfo +
    /// gated NAR with a hit signal — the mid-stream cancellation
    /// victim) and path B whose narinfo route awaits `b_gate` and then
    /// answers 429 ONCE (the deterministically-sequenced transient),
    /// serving B normally afterwards (the reclaim phase).
    async fn spawn_mixed_upstream(
        a: (String, Vec<u8>),
        b: (String, Vec<u8>),
        key_name: &str,
    ) -> (
        FakeUpstream,
        tokio::sync::mpsc::UnboundedReceiver<()>,
        std::sync::Arc<tokio::sync::Notify>,
        std::sync::Arc<tokio::sync::Notify>,
    ) {
        use axum::http::StatusCode;
        use axum::response::IntoResponse;
        use axum::{Router, routing::get};
        use base64::Engine;
        use sha2::Digest;
        use std::sync::Arc;
        use std::sync::atomic::{AtomicBool, Ordering};

        let seed = [0x45u8; 32];
        let signer = Signer::from_seed(key_name, &seed);
        let pubkey = ed25519_dalek::SigningKey::from_bytes(&seed).verifying_key();
        let trusted_key = format!(
            "{key_name}:{}",
            base64::engine::general_purpose::STANDARD.encode(pubkey.as_bytes())
        );
        let (hit_tx, hit_rx) = tokio::sync::mpsc::unbounded_channel::<()>();
        let a_release = Arc::new(tokio::sync::Notify::new());
        let b_gate = Arc::new(tokio::sync::Notify::new());

        let narinfo_for = |path: &str, nar: &[u8]| {
            let nar_hash: [u8; 32] = sha2::Sha256::digest(nar).into();
            let nar_hash_str = format!(
                "sha256:{}",
                rio_nix::store_path::nixbase32::encode(&nar_hash)
            );
            let fp = fingerprint(path, &nar_hash, nar.len() as u64, &[]);
            let sig = signer.sign(&fp);
            format!(
                "StorePath: {path}\n\
                 URL: nar/{}.nar\n\
                 Compression: none\n\
                 NarHash: {nar_hash_str}\n\
                 NarSize: {}\n\
                 References: \n\
                 Sig: {sig}\n",
                StorePath::parse(path).unwrap().hash_part(),
                nar.len()
            )
        };

        let (a_path, a_nar) = a;
        let (b_path, b_nar) = b;
        let a_hash = StorePath::parse(&a_path).unwrap().hash_part().to_string();
        let b_hash = StorePath::parse(&b_path).unwrap().hash_part().to_string();
        let a_ni = narinfo_for(&a_path, &a_nar);
        let b_ni = narinfo_for(&b_path, &b_nar);
        let b_429_pending = Arc::new(AtomicBool::new(true));

        let a_rel = Arc::clone(&a_release);
        let bg = Arc::clone(&b_gate);
        let bp = Arc::clone(&b_429_pending);
        let app = Router::new()
            .route(
                "/nix-cache-info",
                get(|| async { "StoreDir: /nix/store\nWantMassQuery: 1\nPriority: 40\n" }),
            )
            .route(
                &format!("/{a_hash}.narinfo"),
                get(move || async move { a_ni }),
            )
            .route(
                &format!("/nar/{a_hash}.nar"),
                get(move || async move {
                    let _ = hit_tx.send(());
                    a_rel.notified().await;
                    a_nar
                }),
            )
            .route(
                &format!("/{b_hash}.narinfo"),
                get(move || async move {
                    if bp.swap(false, Ordering::SeqCst) {
                        // The sequenced transient: held until the test
                        // confirms A is mid-stream, then a bare 429.
                        bg.notified().await;
                        return (StatusCode::TOO_MANY_REQUESTS, String::new()).into_response();
                    }
                    b_ni.into_response()
                }),
            )
            .route(
                &format!("/nar/{b_hash}.nar"),
                get(move || async move { b_nar }),
            );

        let listener = tokio::net::TcpListener::bind(SocketAddr::from(([127, 0, 0, 1], 0)))
            .await
            .unwrap();
        let addr = listener.local_addr().unwrap();
        let task = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        (
            FakeUpstream {
                url: format!("http://{addr}"),
                trusted_key,
                _task: task,
            },
            hit_rx,
            a_release,
            b_gate,
        )
    }

    proptest::proptest! {
        // r[verify store.materialize.path-fold+1]
        /// W-1a (fold-level permutation invariance): the backlog tier
        /// fold over an arbitrary completed abort-evidence multiset is
        /// input-order invariant, INCLUDING the within-tier merge — and
        /// the law is re-derived independently: charge dominates
        /// transient (the kernel tier table); the wire representative
        /// is the lexicographically-first path of the winning tier
        /// (never first-dequeued); the transient tier carries the MAX
        /// retry_after across completed transient abort-grades.
        #[test]
        fn fold_abort_evidence_is_input_order_invariant(
            evidence in proptest::collection::vec(
                (0usize..5, proptest::bool::ANY, proptest::option::of(0u64..400)),
                1..6,
            ),
            rot in 0usize..6,
        ) {
            let mk = |v: &[(usize, bool, Option<u64>)]| -> Vec<AbortEvidence> {
                v.iter()
                    .map(|(p, charge, ra)| AbortEvidence {
                        path: format!("/nix/store/{p:032}-x"),
                        grade: if *charge {
                            AbortDisposition::Charge { detail: format!("charge {p}") }
                        } else {
                            AbortDisposition::Transient {
                                class: "rate_limited",
                                detail: format!("transient {p}"),
                                retry_after: ra.map(Duration::from_secs),
                            }
                        },
                    })
                    .collect()
            };
            let base = mk(&evidence);
            let mut rotated = evidence.clone();
            let len = evidence.len();
            rotated.rotate_left(rot % len);
            let alt = mk(&rotated);
            let a = fold_abort_evidence(&base);
            let b = fold_abort_evidence(&alt);
            proptest::prop_assert_eq!(&a, &b, "fold must be input-order invariant");

            let any_charge = evidence.iter().any(|(_, c, _)| *c);
            match a.outcome.as_ref().unwrap() {
                materialization_outcome::Outcome::InfraFailure(f) => {
                    proptest::prop_assert!(any_charge, "InfraFailure without charge evidence");
                    let rep = evidence
                        .iter()
                        .filter(|(_, c, _)| *c)
                        .map(|(p, _, _)| *p)
                        .min()
                        .unwrap();
                    proptest::prop_assert!(
                        f.detail.contains(&format!("charge {rep}")),
                        "representative must be the lexicographically-first charge path; got {}",
                        f.detail
                    );
                }
                materialization_outcome::Outcome::RetryLater(r) => {
                    proptest::prop_assert!(!any_charge, "charge must dominate transient");
                    let rep = evidence.iter().map(|(p, _, _)| *p).min().unwrap();
                    proptest::prop_assert!(
                        r.detail.contains(&format!("transient {rep}")),
                        "representative must be the lexicographically-first transient path; got {}",
                        r.detail
                    );
                    let max_ra = evidence.iter().filter_map(|(_, _, ra)| *ra).max().unwrap_or(0);
                    proptest::prop_assert_eq!(
                        r.retry_after_secs, max_ra,
                        "the transient tier carries the MAX retry_after"
                    );
                }
                other => proptest::prop_assert!(false, "unexpected outcome class {other:?}"),
            }
        }
    }

    // r[verify store.materialize.path-fold+1]
    /// W-2(a) window bound: concurrent in-flight path resolutions
    /// never exceed F. Six cold wanted paths, all NAR fetches gated,
    /// F = 4: exactly four fetches reach the upstream (high-water ==
    /// F) and the fifth does NOT start until a slot frees. E-4 rides
    /// the same harness: the walk then completes with ALL six served.
    #[tokio::test]
    async fn window_bounds_inflight_paths_at_fanout() {
        let db = TestDb::new(&crate::MIGRATOR).await;
        let tenant = seed_tenant(&db.pool, "mat-w2").await;

        let paths: Vec<String> = (0..6).map(|i| store_path(10 + i, "w2-path")).collect();
        let (nar, _) = rio_test_support::fixtures::make_nar(b"w2 contents");
        let served: Vec<(String, Vec<u8>, Vec<String>)> = paths
            .iter()
            .map(|p| (p.clone(), nar.clone(), vec![]))
            .collect();
        let (upstream, mut hit_rx, release) = spawn_gated_upstream(served, "cache.w2").await;
        wire_upstream(&db.pool, tenant, &upstream).await;

        let outputs: Vec<(String, &str)> = paths
            .iter()
            .enumerate()
            .map(|(i, p)| (format!("o{i}"), p.as_str()))
            .collect();
        let outputs_ref: Vec<(&str, &str)> =
            outputs.iter().map(|(n, p)| (n.as_str(), *p)).collect();
        let seeded = seed_job(
            &db.pool,
            "mat-w2-drv",
            &outputs_ref,
            Some(tenant),
            Some(tenant),
            &[],
        )
        .await;

        let ctx = make_ctx(db.pool.clone()); // F = 4
        let job = tokio::spawn({
            let claimed = seeded.claimed.clone();
            async move {
                execute_job(&ctx, &claimed, admitted(&ctx))
                    .await
                    .into_outcome()
            }
        });

        // Exactly F fetches start.
        let mut hits = 0usize;
        while hits < 4 {
            tokio::time::timeout(Duration::from_secs(10), hit_rx.recv())
                .await
                .expect("first four fetches must start")
                .expect("hit channel open");
            hits += 1;
        }
        tokio::time::sleep(Duration::from_millis(400)).await;
        assert!(
            hit_rx.try_recv().is_err(),
            "a fifth path resolution started with the window at F=4 \
             (window bound violated)"
        );

        // Release until the walk completes; count the late spawns.
        let releaser = tokio::spawn({
            let release = std::sync::Arc::clone(&release);
            async move {
                loop {
                    release.notify_one();
                    tokio::time::sleep(Duration::from_millis(25)).await;
                }
            }
        });
        let outcome = tokio::time::timeout(Duration::from_secs(30), job)
            .await
            .expect("walk must complete once gates release")
            .unwrap();
        releaser.abort();
        while hit_rx.try_recv().is_ok() {
            hits += 1;
        }
        assert_eq!(hits, 6, "every path fetched exactly once");
        let success = outcome_success(&outcome)
            .unwrap_or_else(|| panic!("expected Success, got {outcome:?}"));
        assert_eq!(
            success.ingested_paths.len() + success.verified_paths.len(),
            6,
            "all six paths covered"
        );
    }

    // r[verify store.materialize.path-fold+1]
    // r[verify store.materialize.progress-monotone+1]
    /// W-1b (same-multiset schedules) + W-5 (floor law under reorder,
    /// both clauses): two jobs with byte-identical 3-path multisets
    /// complete under DIFFERENT schedules (completion order 2,3,1 vs
    /// 1,2,3 — driven by per-path gates). Both fold to the same
    /// outcome class with the full covered set; the committed floor
    /// equals the sum of served sizes under EITHER order (i); and the
    /// OBSERVED emission trace — through the real driver-routed
    /// adapter, ≥2 concurrent emitters with interleaved commits —
    /// never steps below the committed floor at any point (ii).
    #[tokio::test]
    async fn completion_order_is_walk_invisible_and_floor_holds() {
        let db = TestDb::new(&crate::MIGRATOR).await;
        let tenant = seed_tenant(&db.pool, "mat-w5").await;

        // Distinct sizes, each > the 16 KiB test progress interval so
        // provisional ticks flow during the fetch.
        let contents: [Vec<u8>; 3] = [vec![1u8; 20_000], vec![2u8; 28_000], vec![3u8; 36_000]];
        let mut expected_sum = 0u64;

        for (run, order) in [(0usize, [1usize, 2, 0]), (1usize, [0, 1, 2])] {
            let paths: Vec<String> = (0..3)
                .map(|i| store_path(20 + (run * 3 + i) as u8, "w5-path"))
                .collect();
            let nars: Vec<Vec<u8>> = contents
                .iter()
                .map(|c| rio_test_support::fixtures::make_nar(c).0)
                .collect();
            expected_sum = nars.iter().map(|n| n.len() as u64).sum();
            let served: Vec<(String, Vec<u8>, Vec<String>)> = paths
                .iter()
                .zip(nars.iter())
                .map(|(p, n)| (p.clone(), n.clone(), vec![]))
                .collect();
            let (upstream, gates) =
                spawn_pathgated_upstream(served, &format!("cache.w5-{run}")).await;
            wire_upstream(&db.pool, tenant, &upstream).await;

            let outputs: Vec<(String, &str)> = paths
                .iter()
                .enumerate()
                .map(|(i, p)| (format!("o{i}"), p.as_str()))
                .collect();
            let outputs_ref: Vec<(&str, &str)> =
                outputs.iter().map(|(n, p)| (n.as_str(), *p)).collect();
            let seeded = seed_job(
                &db.pool,
                &format!("mat-w5-drv-{run}"),
                &outputs_ref,
                Some(tenant),
                Some(tenant),
                &[],
            )
            .await;

            let events: std::sync::Arc<std::sync::Mutex<Vec<(u64, u64, String)>>> =
                std::sync::Arc::default();
            let sink = std::sync::Arc::clone(&events);
            let ctx = make_ctx(db.pool.clone()); // F = 4: all three in flight
            let job = tokio::spawn({
                let claimed = seeded.claimed.clone();
                async move {
                    execute_job_with_progress(&ctx, &claimed, admitted(&ctx), move |d, e, u| {
                        sink.lock().unwrap().push((d, e, u.to_string()));
                    })
                    .await
                    .into_outcome()
                }
            });

            // Release in the schedule's order, pacing on the commit
            // ticks so completion order is the schedule's order. The
            // first two releases land close together so two fetches
            // stream concurrently (interleaved emitters for (ii)).
            let committed =
                |evs: &[(u64, u64, String)]| evs.iter().filter(|(_, _, u)| u.is_empty()).count();
            for (k, &idx) in order.iter().enumerate() {
                // Keep the released fetch streaming while the next
                // release lands (concurrent emitters), but pace the
                // ORDER by waiting for the previous commit.
                if k > 0 {
                    let want = k;
                    let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
                    while committed(&events.lock().unwrap()) < want {
                        assert!(
                            tokio::time::Instant::now() < deadline,
                            "commit {want} never landed"
                        );
                        tokio::time::sleep(Duration::from_millis(10)).await;
                    }
                }
                let release = std::sync::Arc::clone(&gates[&paths[idx]]);
                // Paced re-notify until the fetch consumes the gate.
                tokio::spawn(async move {
                    for _ in 0..200 {
                        release.notify_one();
                        tokio::time::sleep(Duration::from_millis(20)).await;
                    }
                });
            }

            let outcome = tokio::time::timeout(Duration::from_secs(30), job)
                .await
                .expect("walk must complete")
                .unwrap();
            let success = outcome_success(&outcome)
                .unwrap_or_else(|| panic!("run {run}: expected Success, got {outcome:?}"));
            let mut covered: Vec<String> = success
                .ingested_paths
                .iter()
                .chain(success.verified_paths.iter())
                .cloned()
                .collect();
            covered.sort();
            let mut want = paths.clone();
            want.sort();
            assert_eq!(covered, want, "run {run}: full covered set");

            let evs = events.lock().unwrap().clone();
            // (i) the committed floor equals the sum of served sizes
            // regardless of completion order: the LAST commit is the
            // job total.
            let last_commit = evs
                .iter()
                .rfind(|(_, _, u)| u.is_empty())
                .expect("at least one commit");
            assert_eq!(
                (last_commit.0, last_commit.1),
                (expected_sum, expected_sum),
                "run {run}: final floor == sum of served nar sizes"
            );
            // (ii) the OBSERVED trace never steps below the committed
            // floor at any point (commits are the uri=="" events; the
            // floor at each event is the max committed before it).
            let mut floor = 0u64;
            for (i, (d, e, u)) in evs.iter().enumerate() {
                assert!(
                    *d >= floor,
                    "run {run}: event {i} done {d} below committed floor {floor} \
                     (trace: {evs:?})"
                );
                assert!(*d <= *e, "run {run}: event {i} done {d} > expected {e}");
                if u.is_empty() {
                    floor = floor.max(*d);
                }
            }
        }
        assert!(expected_sum > 0, "fixture sanity");
    }

    // r[verify store.materialize.path-fold+1]
    /// W-1c (abort-first lawfulness) + W-6a (cancellation safety +
    /// reclaim): path A is mid-NAR-stream when sibling B completes
    /// with a sequenced transient (bare 429). The abort latch cancels
    /// A; the outcome is the lawful fold of the ACTUALLY-completed
    /// subset — RetryLater{class=rate_limited}, never a charge for
    /// the cancelled sibling; A contributes ZERO cells and ZERO floor
    /// trace (no commit event); A's placeholder is reaped by the
    /// drop-guard and the NEXT attempt recovers both paths end-to-end.
    #[tokio::test]
    async fn abort_latch_cancels_siblings_lawfully_and_next_attempt_recovers() {
        let db = TestDb::new(&crate::MIGRATOR).await;
        let tenant = seed_tenant(&db.pool, "mat-w6a").await;

        let a_path = store_path(30, "w6a-cancelled");
        let b_path = store_path(31, "w6a-transient");
        let (a_nar, _) = rio_test_support::fixtures::make_nar(&vec![7u8; 40_000]);
        let (b_nar, _) = rio_test_support::fixtures::make_nar(b"w6a-b");
        let (upstream, mut a_hit, a_release, b_gate) = spawn_mixed_upstream(
            (a_path.clone(), a_nar.clone()),
            (b_path.clone(), b_nar.clone()),
            "cache.w6a",
        )
        .await;
        wire_upstream(&db.pool, tenant, &upstream).await;

        let seeded = seed_job(
            &db.pool,
            "mat-w6a-drv",
            &[("a", a_path.as_str()), ("b", b_path.as_str())],
            Some(tenant),
            Some(tenant),
            &[],
        )
        .await;

        let events: std::sync::Arc<std::sync::Mutex<Vec<(u64, u64, String)>>> =
            std::sync::Arc::default();
        let sink = std::sync::Arc::clone(&events);
        let ctx = make_ctx(db.pool.clone());
        let job = tokio::spawn({
            let claimed = seeded.claimed.clone();
            async move {
                execute_job_with_progress(&ctx, &claimed, admitted(&ctx), move |d, e, u| {
                    sink.lock().unwrap().push((d, e, u.to_string()));
                })
                .await
                .into_outcome()
            }
        });

        // A is mid-stream (claimed, GET sent, body gated)...
        tokio::time::timeout(Duration::from_secs(10), a_hit.recv())
            .await
            .expect("A must reach its NAR fetch")
            .expect("hit channel open");
        // ...now release B's sequenced 429: the abort latch fires
        // while A is in flight.
        b_gate.notify_one();

        let outcome = tokio::time::timeout(Duration::from_secs(15), job)
            .await
            .expect("latch must cancel A, not wait for it")
            .unwrap();
        match outcome.outcome.as_ref().unwrap() {
            materialization_outcome::Outcome::RetryLater(r) => {
                assert_eq!(
                    r.class, "rate_limited",
                    "the transient's class rides the wire"
                );
            }
            other => panic!(
                "abort-first fold must be the lawful fold of the completed \
                 subset (RetryLater), got {other:?}"
            ),
        }
        // The cancelled sibling left no floor trace: zero commits.
        assert!(
            events.lock().unwrap().iter().all(|(_, _, u)| !u.is_empty()),
            "a cancelled path's streamed bytes must never commit; events={:?}",
            events.lock().unwrap()
        );
        assert_eq!(
            pin_count(&db.pool, "mat-w6a-drv", "materialization").await,
            0,
            "no path served, no pin"
        );

        // W-6a reclaim: the drop-guard reaps A's placeholder...
        let a_hash = StorePath::parse(&a_path).unwrap().sha256_digest();
        let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
        loop {
            let age = metadata::manifest_uploading_age(&db.pool, &a_hash)
                .await
                .unwrap();
            if age.is_none() {
                break;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "cancelled sibling's placeholder never reaped"
            );
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        // ...and the next attempt recovers BOTH paths (B's 429 was
        // once-only; A's gate is re-notified until consumed).
        let releaser = tokio::spawn(async move {
            loop {
                a_release.notify_one();
                tokio::time::sleep(Duration::from_millis(25)).await;
            }
        });
        let ctx2 = make_ctx(db.pool.clone());
        let outcome2 = tokio::time::timeout(
            Duration::from_secs(30),
            execute_job(&ctx2, &seeded.claimed, admitted(&ctx2)),
        )
        .await
        .expect("second attempt must complete")
        .into_outcome();
        releaser.abort();
        let success = outcome_success(&outcome2)
            .unwrap_or_else(|| panic!("second attempt: expected Success, got {outcome2:?}"));
        assert_eq!(
            success.ingested_paths.len() + success.verified_paths.len(),
            2,
            "both paths recovered after the cancelled attempt"
        );
        assert_eq!(
            pin_count(&db.pool, "mat-w6a-drv", "materialization").await,
            2,
            "pin-at-ingest on the recovery attempt"
        );
    }

    // r[verify store.materialize.path-fold+1]
    /// W-6b (the moka retry leg — GATES the F>1 cancellation policy):
    /// cancel a singleflight LEADER mid-NAR-fetch while a coalesced
    /// second caller awaits the same `(tenant, path)`. moka 0.12.15
    /// does NOT adopt the dropped leader's init future — the waiter
    /// RETRIES with its OWN init future. The survivor must complete
    /// `Ok` through that retry; a transient `Raced` (the dropped
    /// leader's placeholder, pre-reap) is lawful and resolves on
    /// re-call; a stranded or otherwise-erroring waiter is not.
    #[tokio::test]
    async fn cancelled_leader_recovers_coalesced_waiter_by_retry() {
        let db = TestDb::new(&crate::MIGRATOR).await;
        let tenant = seed_tenant(&db.pool, "mat-w6b").await;
        let path = store_path(33, "w6b-path");
        let (nar, _) = rio_test_support::fixtures::make_nar(b"w6b contents");
        let (upstream, mut hit_rx, release) =
            spawn_gated_upstream(vec![(path.clone(), nar.clone(), vec![])], "cache.w6b").await;
        wire_upstream(&db.pool, tenant, &upstream).await;

        let sub = std::sync::Arc::new(
            Substituter::new(db.pool.clone(), None).with_http_client(sandbox_http()),
        );
        let leader = tokio::spawn({
            let s = std::sync::Arc::clone(&sub);
            let p = path.clone();
            async move { s.try_substitute(tenant, &p).await }
        });
        tokio::time::timeout(Duration::from_secs(10), hit_rx.recv())
            .await
            .expect("leader must reach its NAR fetch")
            .expect("hit channel open");
        let waiter = tokio::spawn({
            let s = std::sync::Arc::clone(&sub);
            let p = path.clone();
            async move { s.try_substitute(tenant, &p).await }
        });
        // Let the waiter coalesce on the moka key, then cancel the
        // leader mid-fetch.
        tokio::time::sleep(Duration::from_millis(200)).await;
        leader.abort();
        let _ = leader.await;

        // The retry's own fetch awaits the same gate: keep releasing.
        let releaser = tokio::spawn(async move {
            loop {
                release.notify_one();
                tokio::time::sleep(Duration::from_millis(25)).await;
            }
        });
        // The waiter must RESOLVE (never strand): Ok directly, or a
        // lawful BOUNDED transient while the drop settles — re-calls
        // (the executor's re-arm) converge to Ok. Two lawful
        // transients, both observed and both UNCACHED (the
        // merged_bug_044 law held throughout — an error here must
        // never become a 30s-cached definitive miss):
        // - `Raced`: the dropped leader's placeholder pre-reap;
        // - `Fetch` (all-errored fold): the cancelled leader's
        //   mid-body drop poisons the SHARED reqwest pooled
        //   connection, and the survivor's retry can hit the dead
        //   connection once (recorded WO-R7-2B finding: a spurious
        //   once-off charge-class error is possible on the retry
        //   leg; bounded, uncached, self-healing — disclosed in the
        //   wave-log rather than deferring cancellation).
        let first = tokio::time::timeout(Duration::from_secs(15), waiter)
            .await
            .expect("coalesced waiter must never strand after leader drop")
            .unwrap();
        let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
        let mut latest = first;
        let got = loop {
            match latest {
                Ok(Some(info)) => break info,
                Ok(None) => panic!("retry leg must not cache a miss for a servable path"),
                Err(
                    e @ (crate::substitute::SubstituteError::Raced
                    | crate::substitute::SubstituteError::Fetch(_)),
                ) => {
                    assert!(
                        tokio::time::Instant::now() < deadline,
                        "transient window never cleared (guard reap missing?); last: {e:?}"
                    );
                    tokio::time::sleep(Duration::from_millis(100)).await;
                    latest = sub.try_substitute(tenant, &path).await;
                }
                Err(other) => panic!(
                    "survivor must recover via its own retried init future; \
                     got unlawful error {other:?}"
                ),
            }
        };
        releaser.abort();
        assert_eq!(
            got.nar_size,
            nar.len() as u64,
            "the survivor served the real path"
        );
    }

    // r[verify store.materialize.gate-share+1]
    /// W-4 pool ceiling: executor-held admission permits never exceed
    /// effective_cap / 2 — the d1f18610d invariant made STRUCTURAL
    /// (the pod path-slot pool), not arithmetic. Two concurrent walks
    /// x F=4 = 8 cold gated paths against an 8-permit gate: without
    /// the pool the executor holds the ENTIRE gate (RPC miss traffic
    /// starved, 25s queue then RESOURCE_EXHAUSTED — the n=64
    /// rejection's failure mode reached through fan-out); with the
    /// pool, held permits stay <= P = cap/2 and both walks still
    /// complete.
    #[tokio::test]
    async fn executor_admission_draw_bounded_to_half_the_gate() {
        let db = TestDb::new(&crate::MIGRATOR).await;
        let tenant = seed_tenant(&db.pool, "mat-w4").await;

        let gate_cap = 8usize;
        let gate = crate::admission::AdmissionGate::new(gate_cap);
        let paths: Vec<String> = (0..8).map(|i| store_path(40 + i, "w4-path")).collect();
        let (nar, _) = rio_test_support::fixtures::make_nar(b"w4 contents");
        let served: Vec<(String, Vec<u8>, Vec<String>)> = paths
            .iter()
            .map(|p| (p.clone(), nar.clone(), vec![]))
            .collect();
        let (upstream, mut hit_rx, release) = spawn_gated_upstream(served, "cache.w4").await;
        wire_upstream(&db.pool, tenant, &upstream).await;

        let substituter = std::sync::Arc::new(
            Substituter::new(db.pool.clone(), None)
                .with_http_client(sandbox_http())
                .with_admission_gate(gate.clone()),
        );
        // Two jobs of four paths each (n=2 walks, F=4: nominal demand
        // n x F = 8 = the WHOLE gate).
        let mut jobs = Vec::new();
        for (j, chunk) in paths.chunks(4).enumerate() {
            let outputs: Vec<(String, &str)> = chunk
                .iter()
                .enumerate()
                .map(|(i, p)| (format!("o{i}"), p.as_str()))
                .collect();
            let outputs_ref: Vec<(&str, &str)> =
                outputs.iter().map(|(n, p)| (n.as_str(), *p)).collect();
            let seeded = seed_job(
                &db.pool,
                &format!("mat-w4-drv-{j}"),
                &outputs_ref,
                Some(tenant),
                Some(tenant),
                &[],
            )
            .await;
            jobs.push(seeded);
        }
        // ONE pod pool at P = cap/2 through the REAL derivation fn —
        // shared by both walks like main.rs shares it across workers.
        let pool_p = crate::config::derive_executor_path_slots(gate_cap);
        assert_eq!(pool_p, gate_cap / 2);
        let path_slots = PathSlotPool::new(pool_p);
        let mk_ctx = || {
            ExecutorContext::new(
                db.pool.clone(),
                std::sync::Arc::clone(&substituter),
                4,
                path_slots.clone(),
            )
        };
        let handles: Vec<_> = jobs
            .iter()
            .map(|j| {
                let ctx = mk_ctx();
                let claimed = j.claimed.clone();
                tokio::spawn(async move {
                    execute_job(&ctx, &claimed, admitted(&ctx))
                        .await
                        .into_outcome()
                })
            })
            .collect();

        // Sample the executor's admission draw until the in-flight
        // population quiesces (no new gated fetch for 600 ms), then
        // pin the high-water.
        let mut held_high = 0usize;
        let mut last_hit = tokio::time::Instant::now();
        let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
        loop {
            held_high = held_high.max(gate_cap - gate.semaphore().available_permits());
            if hit_rx.try_recv().is_ok() {
                last_hit = tokio::time::Instant::now();
            } else if last_hit.elapsed() > Duration::from_millis(600) {
                break;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "fetches never quiesced"
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert!(
            held_high <= gate_cap / 2,
            "executor-held admission permits hit {held_high} of {gate_cap} — the \
             executor saturated its own gate (>= cap/2 must stay available to \
             RPC miss traffic; the gate-share invariant is inverted at F>1 \
             without the path-slot pool)"
        );

        // Liveness: release everything; both walks complete fully.
        let releaser = tokio::spawn(async move {
            loop {
                release.notify_one();
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        });
        for h in handles {
            let outcome = tokio::time::timeout(Duration::from_secs(30), h)
                .await
                .expect("walk must complete")
                .unwrap();
            let success = outcome_success(&outcome)
                .unwrap_or_else(|| panic!("expected Success, got {outcome:?}"));
            assert_eq!(
                success.ingested_paths.len() + success.verified_paths.len(),
                4,
                "all four paths covered"
            );
        }
        releaser.abort();
    }

    // r[verify store.materialize.gate-share+1]
    /// W-2(b) baseline liveness (TRUE RED against the first-path-only
    /// slot rule): pool P=1, walk A's claim carries the slot into a
    /// two-path frontier; while a1 is in flight a FOREIGN waiter
    /// queues on the pool. a1 completes → the fair semaphore hands
    /// the freed slot to the QUEUED waiter (yield law — the queued
    /// entry beats any later acquire) → A sits at width 0 with a
    /// nonempty frontier. Under the width-1 BASELINE INVARIANT A's
    /// next acquire is blocking-FIFO and the walk finishes once the
    /// waiter releases; under the first-path-only strawman A never
    /// re-queues and wedges.
    ///
    /// DISCLOSED RE-DERIVATION (bug_102, recorded as a divergence in
    /// the owning commit): the original two-walk topology — walk B
    /// parked at its FIRST acquire on a saturated P=1 pool — is
    /// UNREPRESENTABLE post-close (a claim cannot exist without
    /// holding a slot: B would never have been admitted), so the
    /// queued contender is a raw foreign waiter; the certified laws
    /// (mid-walk width-1 baseline re-queue + the yield direction) are
    /// unchanged, and the histogram pins exactly ONE baseline sample
    /// (A's mid-walk re-queue; the carried first spawn contributes
    /// none).
    #[tokio::test]
    async fn drained_walk_requeues_baseline_and_completes() {
        use metrics_util::debugging::{DebugValue, DebuggingRecorder};
        let rec = DebuggingRecorder::new();
        let snap = rec.snapshotter();
        let _guard = metrics::set_default_local_recorder(&rec);

        let db = TestDb::new(&crate::MIGRATOR).await;
        let tenant = seed_tenant(&db.pool, "mat-w2b").await;

        let a1 = store_path(50, "w2b-a1");
        let a2 = store_path(51, "w2b-a2");
        let (nar, _) = rio_test_support::fixtures::make_nar(b"w2b contents");
        let (upstream, gates) = spawn_pathgated_upstream(
            vec![
                (a1.clone(), nar.clone(), vec![]),
                (a2.clone(), nar.clone(), vec![]),
            ],
            "cache.w2b",
        )
        .await;
        wire_upstream(&db.pool, tenant, &upstream).await;

        let job_a = seed_job(
            &db.pool,
            "mat-w2b-drv-a",
            &[("a1", a1.as_str()), ("a2", a2.as_str())],
            Some(tenant),
            Some(tenant),
            &[],
        )
        .await;

        let substituter = std::sync::Arc::new(
            Substituter::new(db.pool.clone(), None).with_http_client(sandbox_http()),
        );
        // ONE slot: the steady-state contention regime, minimized.
        let pool1 = PathSlotPool::new(1);
        let ctx_a = ExecutorContext::new(
            db.pool.clone(),
            std::sync::Arc::clone(&substituter),
            4,
            pool1.clone(),
        );

        // A's claim carries the pool's only slot in (slot ≺ claim);
        // a1 spawns on it and parks at its gated fetch.
        let adm_a = admitted(&ctx_a);
        let ca = job_a.claimed.clone();
        let ha = tokio::spawn(async move { execute_job(&ctx_a, &ca, adm_a).await.into_outcome() });
        tokio::time::sleep(Duration::from_millis(400)).await;

        // The foreign waiter queues on the saturated pool (raw — it
        // must not pollute the walk's baseline instrumentation).
        let (waiter_got_tx, waiter_got_rx) = tokio::sync::oneshot::channel::<()>();
        let (waiter_release_tx, waiter_release_rx) = tokio::sync::oneshot::channel::<()>();
        let waiter = tokio::spawn({
            let slots = std::sync::Arc::clone(&pool1.slots);
            async move {
                let permit = slots.acquire_owned().await.expect("pool never closed");
                let _ = waiter_got_tx.send(());
                let _ = waiter_release_rx.await;
                drop(permit);
            }
        });
        tokio::time::sleep(Duration::from_millis(100)).await;

        // Release a1: A completes its sole in-flight path; the freed
        // slot goes to the QUEUED waiter (yield law); A re-queues at
        // the baseline (the invariant under test).
        let g = std::sync::Arc::clone(&gates[&a1]);
        tokio::spawn(async move {
            for _ in 0..400 {
                g.notify_one();
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        });
        tokio::time::timeout(Duration::from_secs(10), waiter_got_rx)
            .await
            .expect("the queued waiter receives a1's freed slot (yield law)")
            .unwrap();
        // A is now width 0 with frontier [a2], queued blocking-FIFO.
        // Release the waiter, then a2's gate — A must finish.
        let _ = waiter_release_tx.send(());
        let g2 = std::sync::Arc::clone(&gates[&a2]);
        tokio::spawn(async move {
            for _ in 0..400 {
                g2.notify_one();
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        });

        let outcome = tokio::time::timeout(Duration::from_secs(30), ha)
            .await
            .unwrap_or_else(|_| {
                panic!(
                    "walk A wedged: width-0 with a nonempty frontier and no \
                     completion event ever arrives again (the first-path-only \
                     slot rule's permanent mid-walk stall)"
                )
            })
            .unwrap();
        let success = outcome_success(&outcome)
            .unwrap_or_else(|| panic!("walk A: expected Success, got {outcome:?}"));
        assert_eq!(success.ingested_paths.len(), 2, "walk A served both paths");
        assert_eq!(
            pin_count(&db.pool, "mat-w2b-drv-a", "materialization").await,
            2,
            "walk A finished BOTH paths (baseline re-queue worked)"
        );
        // Exactly ONE baseline sample: A's mid-walk re-queue (the
        // carried first spawn contributes none) — snapshot once.
        let mut samples = 0usize;
        for (ck, _, _, v) in snap.snapshot().into_vec() {
            if ck.key().name() == "rio_store_executor_path_slot_baseline_wait_seconds" {
                let DebugValue::Histogram(h) = v else {
                    continue;
                };
                samples = h.len();
            }
        }
        assert_eq!(samples, 1, "one mid-walk baseline re-queue, no more");
        waiter.abort();
    }

    // r[verify store.materialize.path-fold+1]
    // r[verify store.materialize.gate-share+1]
    /// R1-003 (merged_bug_003, TRUE RED pre-fix) / W-003a: a walk
    /// whose remaining frontier is DUP-ONLY completes WITHOUT entering
    /// the baseline FIFO — certified two ways at once: completion
    /// under an externally-held pool (the wait would be unbounded
    /// pre-fix) AND a baseline-wait histogram count of exactly one
    /// sample per REAL spawn (the metric the defect inflated, now the
    /// witness — counting, not wall-clock).
    ///
    /// Diamond closure A→{B,C}, B→{D}, C→{D} via production narinfo
    /// references; F = 1, pool capacity 1. C's apply re-enqueues D
    /// after D already spawned (push-time check vs spawn-time insert —
    /// the two timestamps), leaving a dup-only frontier after D
    /// completes. The test queues a raw holder on the pool while D is
    /// in flight: the fair FIFO hands D's freed slot to the holder, so
    /// a post-fix walk must finish WITHOUT another acquire.
    ///
    /// Pre-fix red: the walk parks in acquire_baseline on the dup-only
    /// frontier behind the test-held slot — no outcome within the
    /// deadline, a baseline FIFO entry for zero spawnable work.
    /// Post-fix: the walk breaks at width 0 with an empty frontier,
    /// outcome reported, histogram count == 4 (one per real spawn,
    /// none for the dup tail), without the held slot ever releasing.
    #[tokio::test]
    async fn dup_only_frontier_completes_without_baseline_acquire() {
        use metrics_util::debugging::{DebugValue, DebuggingRecorder};
        let rec = DebuggingRecorder::new();
        let snap = rec.snapshotter();
        let _guard = metrics::set_default_local_recorder(&rec);

        let db = TestDb::new(&crate::MIGRATOR).await;
        let tenant = seed_tenant(&db.pool, "dup-frontier").await;

        let a = store_path(70, "dupf-a");
        let b = store_path(71, "dupf-b");
        let c = store_path(72, "dupf-c");
        let d = store_path(73, "dupf-d");
        let (nar, _) = rio_test_support::fixtures::make_nar(b"dupf contents");
        let (upstream, gates) = spawn_pathgated_upstream(
            vec![
                (a.clone(), nar.clone(), vec![b.clone(), c.clone()]),
                (b.clone(), nar.clone(), vec![d.clone()]),
                (c.clone(), nar.clone(), vec![d.clone()]),
                (d.clone(), nar.clone(), vec![]),
            ],
            "cache.dupf",
        )
        .await;
        wire_upstream(&db.pool, tenant, &upstream).await;
        let seeded = seed_job(
            &db.pool,
            "dupf-drv",
            &[("out", a.as_str())],
            Some(tenant),
            Some(tenant),
            &[],
        )
        .await;

        // F = 1 (serial spawn order A,B,C,D), pool capacity 1.
        let pool1 = PathSlotPool::new(1);
        let ctx = ExecutorContext::new(
            db.pool.clone(),
            std::sync::Arc::new(
                Substituter::new(db.pool.clone(), None).with_http_client(sandbox_http()),
            ),
            1,
            pool1.clone(),
        );
        let events: std::sync::Arc<std::sync::Mutex<Vec<(u64, u64, String)>>> =
            std::sync::Arc::default();
        let sink = std::sync::Arc::clone(&events);
        let job = tokio::spawn({
            let claimed = seeded.claimed.clone();
            async move {
                execute_job_with_progress(&ctx, &claimed, admitted(&ctx), move |dn, e, u| {
                    sink.lock().unwrap().push((dn, e, u.to_string()));
                })
                .await
                .into_outcome()
            }
        });

        // Release A, B, C (paced re-notifies); D's gate stays closed.
        for path in [&a, &b, &c] {
            let g = std::sync::Arc::clone(&gates[path]);
            tokio::spawn(async move {
                for _ in 0..400 {
                    g.notify_one();
                    tokio::time::sleep(Duration::from_millis(20)).await;
                }
            });
        }
        // Wait for 3 commits (A, B, C applied — C's apply re-enqueued
        // D pre-fix), then for D's spawn to hold the pool's only slot.
        let committed =
            |evs: &[(u64, u64, String)]| evs.iter().filter(|(_, _, u)| u.is_empty()).count();
        let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
        while committed(&events.lock().unwrap()) < 3 {
            assert!(
                tokio::time::Instant::now() < deadline,
                "commits A/B/C never landed"
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        while pool1.slots.available_permits() > 0 {
            assert!(
                tokio::time::Instant::now() < deadline,
                "D's spawn never took the slot"
            );
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        // Queue the foreign holder BEHIND D (raw semaphore access —
        // in-module; FIFO puts it ahead of any later walk acquire).
        // Raw on purpose: the holder must not pollute the walk's
        // baseline instrumentation.
        let holder = tokio::spawn({
            let slots = std::sync::Arc::clone(&pool1.slots);
            async move {
                let permit = slots.acquire_owned().await.expect("pool never closed");
                tokio::time::sleep(Duration::from_secs(120)).await;
                drop(permit);
            }
        });
        tokio::time::sleep(Duration::from_millis(100)).await;
        // Release D: its slot goes to the queued holder on completion.
        let g = std::sync::Arc::clone(&gates[&d]);
        tokio::spawn(async move {
            for _ in 0..400 {
                g.notify_one();
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        });

        let outcome = tokio::time::timeout(Duration::from_secs(5), job)
            .await
            .expect(
                "walk parks in acquire_baseline on a dup-only frontier behind the \
                 test-held slot — no outcome within the deadline (a baseline FIFO \
                 entry for zero spawnable work)",
            )
            .unwrap();
        let success = outcome_success(&outcome)
            .unwrap_or_else(|| panic!("expected Success, got {outcome:?}"));
        assert_eq!(
            success.ingested_paths.len(),
            4,
            "all four diamond paths served"
        );
        // Histogram count: one sample per REAL baseline spawn (B,C,D —
        // path A rode the claim's carried slot, bug_102), none for the
        // dup tail — snapshot exactly once.
        let mut samples = 0usize;
        for (ck, _, _, v) in snap.snapshot().into_vec() {
            if ck.key().name() == "rio_store_executor_path_slot_baseline_wait_seconds" {
                let DebugValue::Histogram(h) = v else {
                    continue;
                };
                samples = h.len();
            }
        }
        assert_eq!(
            samples, 3,
            "baseline-wait histogram counts exactly the three real baseline \
             spawns (zero phantom samples for the dup-only tail)"
        );
        assert_eq!(
            pool1.slots.available_permits(),
            0,
            "the foreign holder still holds the slot — the walk finished without it"
        );
        holder.abort();
    }

    /// R2-003 (merged_bug_003, cross-chain cell, TRUE RED pre-fix):
    /// a path arriving via BOTH `new_seeds` and `reseed_references`
    /// in ONE iteration is enqueued once. Choreography: build A
    /// (tenant A) wants only output "out" = W; W's narinfo references
    /// X (a declared output A does not want); A's upstream serves W
    /// (gated) and 404s X, so X settles missing-Reference under
    /// generation 0 and stays `visited`. During W's gate, A goes
    /// terminal and build B (tenant B, upstream serving X) arrives
    /// wanting ALL outputs. Iteration 2: the grown tenant set drains
    /// X from `visited` into `reseed_references` (bug_266) while the
    /// wanted re-read lists X as a new seed — the cross-chain dup.
    ///
    /// Pre-fix red (run + recorded in the owning commit body): the
    /// frontier held X twice; after X spawned once (spawn-time
    /// insert), the second X drained to a dup — a slot was acquired
    /// and dropped unused (the prelude seam counter read 1 against
    /// 0 expected). Post-fix: enqueue-dedup admits X once; X is
    /// fetched exactly once. DISCLOSED MECHANICAL RE-AIM (the round-6
    /// precedent): the unused-drop branch is structurally DELETED by
    /// the close (pop always spawns under enqueue-dedup), so the seam
    /// counter deleted with it — the single-spawn witness is the
    /// upstream hit count, and the dup-enqueue state is pinned
    /// unrepresentable by the pop-side debug_assert + the model-plane
    /// `frontierSpawnable` hold.
    #[tokio::test]
    async fn cross_chain_duplicate_enqueues_once() {
        let db = TestDb::new(&crate::MIGRATOR).await;
        let tenant_a = seed_tenant(&db.pool, "xchain-a").await;
        let tenant_b = seed_tenant(&db.pool, "xchain-b").await;

        let w = store_path(74, "xchain-w");
        let x = store_path(75, "xchain-x");
        let (nar_w, _) = rio_test_support::fixtures::make_nar(b"xchain w");
        let (nar_x, _) = rio_test_support::fixtures::make_nar(b"xchain x");

        // Tenant A: gated upstream serving ONLY W (refs X), 404s X.
        let (up_a, mut hit_rx, release) =
            spawn_gated_upstream(vec![(w.clone(), nar_w, vec![x.clone()])], "cache.xchain-a").await;
        wire_upstream(&db.pool, tenant_a, &up_a).await;
        // Tenant B: gated upstream serving ONLY X (hit-counted).
        let (up_b, mut hit_rx_b, release_b) =
            spawn_gated_upstream(vec![(x.clone(), nar_x, vec![])], "cache.xchain-b").await;
        wire_upstream(&db.pool, tenant_b, &up_b).await;

        // Outputs: out→W, doc→X; build A wants ONLY "out".
        let seeded = seed_job(
            &db.pool,
            "xchain-drv",
            &[("out", w.as_str()), ("doc", x.as_str())],
            Some(tenant_a),
            Some(tenant_a),
            &["out"],
        )
        .await;

        let ctx = make_ctx(db.pool.clone());
        let claimed = seeded.claimed.clone();
        let walk = tokio::spawn(async move {
            execute_job(&ctx, &claimed, admitted(&ctx))
                .await
                .into_outcome()
        });

        // Inside iteration 1's gated W fetch: X has not yet settled
        // (references enter at W's apply), and the tenant set was
        // resolved as {A}.
        tokio::time::timeout(Duration::from_secs(30), hit_rx.recv())
            .await
            .expect("the walk reached tenant A's gated W fetch")
            .expect("gate signal");

        // The growth: A terminal; B live, wanting ALL outputs ('{}').
        sqlx::query("UPDATE builds SET status = 'succeeded' WHERE build_id = $1")
            .bind(seeded.build_id)
            .execute(&db.pool)
            .await
            .unwrap();
        let build_b = Uuid::new_v4();
        sqlx::query("INSERT INTO builds (build_id, tenant_id, status) VALUES ($1, $2, 'active')")
            .bind(build_b)
            .bind(tenant_b)
            .execute(&db.pool)
            .await
            .unwrap();
        sqlx::query("INSERT INTO build_derivations (build_id, derivation_id) VALUES ($1, $2)")
            .bind(build_b)
            .bind(seeded.derivation_id)
            .execute(&db.pool)
            .await
            .unwrap();
        sqlx::query(
            "INSERT INTO build_wanted_outputs (build_id, derivation_id, wanted_output_names) \
             VALUES ($1, $2, '{}')",
        )
        .bind(build_b)
        .bind(seeded.derivation_id)
        .execute(&db.pool)
        .await
        .unwrap();

        release.notify_waiters();
        // X's fetch under B: paced release.
        tokio::spawn(async move {
            for _ in 0..400 {
                release_b.notify_waiters();
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        });
        let outcome = tokio::time::timeout(Duration::from_secs(30), walk)
            .await
            .expect("walk completes")
            .unwrap();
        let success = outcome_success(&outcome)
            .unwrap_or_else(|| panic!("expected Success, got {outcome:?}"));
        let mut got: Vec<String> = success
            .ingested_paths
            .iter()
            .chain(success.verified_paths.iter())
            .cloned()
            .collect();
        got.sort();
        let mut want = vec![w.clone(), x.clone()];
        want.sort();
        assert_eq!(got, want, "W (under A) and X (under B) both covered");

        // Single spawn: X fetched exactly once from B.
        let mut b_hits = 0usize;
        while hit_rx_b.try_recv().is_ok() {
            b_hits += 1;
        }
        assert_eq!(b_hits, 1, "X fetched exactly once (single spawn)");
    }

    // r[verify store.materialize.gate-share+1]
    /// R1-102 (bug_102, TRUE RED pre-fix): a claimed walk must not be
    /// able to park waiting for its FIRST path slot — pre-fix the
    /// width-0 baseline waiter at job start held the CLAIM (an open
    /// attempt window with a running establishment deadline) while
    /// holding zero slots, the exact "slot waiters hold nothing"
    /// violation: the sweep cannot distinguish that queued-healthy
    /// walk from an unreported crash, so it establishes charged and
    /// ladders healthy jobs under the expected helm regime
    /// (n×F = 128 > P = 32).
    ///
    /// Pre-fix red (run + recorded in the owning commit body): the
    /// walk parked in acquire_baseline holding the claim — no
    /// outcome, no slot, attempt window burning (the unreported-crash
    /// establishment shape). Post-fix the parked shape DOES NOT
    /// TYPECHECK — `execute_job` demands a `ClaimAdmission` and the
    /// mint is non-blocking, so there is no first-slot wait state on
    /// a claimed job at all; this re-aimed body pins the runtime half
    /// (no headroom ⇒ no admission ⇒ no claim; an admission HOLDS its
    /// slot; an unconsumed admission returns it).
    #[tokio::test]
    async fn claimed_walk_cannot_park_for_its_first_slot() {
        let pool1 = PathSlotPool::new(1);
        let held = pool1
            .slots
            .clone()
            .try_acquire_owned()
            .expect("fresh pool has the slot");
        assert!(
            pool1.try_admit_claim().is_none(),
            "no slot headroom => no claim admission (the job stays \
             scheduler-listed, claimable by a pod with headroom)"
        );
        drop(held);
        let admission = pool1
            .try_admit_claim()
            .expect("freed headroom admits the claim");
        assert_eq!(
            pool1.slots.available_permits(),
            0,
            "the admission HOLDS the first slot from claim onward"
        );
        drop(admission);
        assert_eq!(
            pool1.slots.available_permits(),
            1,
            "an unconsumed admission returns its slot (empty-frontier drop)"
        );
    }

    // r[verify store.materialize.gate-share+1]
    /// R3-102 (bug_102, TRUE RED pre-fix): the admission slot seeds
    /// the FIRST spawn — a claimed walk's path 1 consumes the slot
    /// carried by the claim, never entering the baseline FIFO.
    /// F = 4, 6-path closure, pool capacity 2: the carried slot
    /// covers spawn 1 and every further width change rides try_widen,
    /// so the baseline-wait histogram stays at ZERO samples for the
    /// whole walk; widening proceeds as before (the W-2(a) shape —
    /// all six served).
    ///
    /// Pre-fix red (run + recorded): the first spawn queued at the
    /// baseline FIFO (histogram count >= 1 — a claim-to-first-spawn
    /// wait inside the attempt window).
    #[tokio::test]
    async fn admission_slot_seeds_the_first_spawn() {
        use metrics_util::debugging::{DebugValue, DebuggingRecorder};
        let rec = DebuggingRecorder::new();
        let snap = rec.snapshotter();
        let _guard = metrics::set_default_local_recorder(&rec);

        let db = TestDb::new(&crate::MIGRATOR).await;
        let tenant = seed_tenant(&db.pool, "adm-seed").await;
        let paths: Vec<String> = (0..6).map(|i| store_path(90 + i, "admseed")).collect();
        let (nar, _) = rio_test_support::fixtures::make_nar(b"admseed contents");
        let served: Vec<(String, Vec<u8>, Vec<String>)> = paths
            .iter()
            .map(|p| (p.clone(), nar.clone(), vec![]))
            .collect();
        let upstream = spawn_multi_upstream(served, "cache.admseed").await;
        wire_upstream(&db.pool, tenant, &upstream).await;
        let outputs: Vec<(String, &str)> = paths
            .iter()
            .enumerate()
            .map(|(i, p)| (format!("o{i}"), p.as_str()))
            .collect();
        let outputs_ref: Vec<(&str, &str)> =
            outputs.iter().map(|(n, p)| (n.as_str(), *p)).collect();
        let seeded = seed_job(
            &db.pool,
            "admseed-drv",
            &outputs_ref,
            Some(tenant),
            Some(tenant),
            &[],
        )
        .await;

        let pool2 = PathSlotPool::new(2);
        let ctx = ExecutorContext::new(
            db.pool.clone(),
            std::sync::Arc::new(
                Substituter::new(db.pool.clone(), None).with_http_client(sandbox_http()),
            ),
            4,
            pool2,
        );
        let outcome = tokio::time::timeout(Duration::from_secs(30), async {
            execute_job(&ctx, &seeded.claimed, admitted(&ctx))
                .await
                .into_outcome()
        })
        .await
        .expect("walk completes");
        let success = outcome_success(&outcome)
            .unwrap_or_else(|| panic!("expected Success, got {outcome:?}"));
        assert_eq!(
            success.ingested_paths.len(),
            6,
            "all six paths served (widening preserved)"
        );
        let mut samples = 0usize;
        for (ck, _, _, v) in snap.snapshot().into_vec() {
            if ck.key().name() == "rio_store_executor_path_slot_baseline_wait_seconds" {
                let DebugValue::Histogram(h) = v else {
                    continue;
                };
                samples = h.len();
            }
        }
        assert_eq!(
            samples, 0,
            "the first spawn queued at the baseline FIFO with the claim open \
             (the carried admission slot must seed it — zero baseline samples)"
        );
    }

    /// Prelude seam smoke (the width-4 red seams): a NO-OP publish
    /// gate is semantics-neutral — the in-use gauge still tracks pool
    /// truth on both edges — and the constructor defaults every test
    /// seam off. The seams' consuming reds live with their owning
    /// closes (probe rendezvous, read/set interleave hook; the
    /// unused-permit counter seam was deleted WITH its branch by the
    /// merged_bug_003 enqueue-dedup close).
    #[tokio::test]
    async fn interleave_seams_default_off_and_noop_gate_is_neutral() {
        use metrics_util::debugging::{DebugValue, DebuggingRecorder};
        let rec = DebuggingRecorder::new();
        let snap = rec.snapshotter();
        let _guard = metrics::set_default_local_recorder(&rec);

        let fired = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let fired_in_gate = std::sync::Arc::clone(&fired);
        let pool = PathSlotPool::new(2).with_publish_gate(std::sync::Arc::new(move || {
            fired_in_gate.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        }));
        let slot = pool.acquire_baseline().await;
        drop(slot);

        // Snapshot EXACTLY ONCE (DebuggingRecorder semantics) and read
        // the gauge's final value: 0 after the paired acquire/drop.
        let mut in_use: Option<f64> = None;
        for (ck, _, _, v) in snap.snapshot().into_vec() {
            if ck.key().name() == "rio_store_executor_path_slots_in_use" {
                let DebugValue::Gauge(g) = v else { continue };
                in_use = Some(g.into_inner());
            }
        }
        assert_eq!(
            in_use,
            Some(0.0),
            "gauge returns to pool truth with a no-op gate installed"
        );
        assert_eq!(
            fired.load(std::sync::atomic::Ordering::SeqCst),
            2,
            "both publish edges route through the seam (acquire + drop)"
        );
    }
}
