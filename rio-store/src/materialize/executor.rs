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

use crate::substitute::{SubstituteError, Substituter};
use crate::visibility::{TenantVisible, TrustedSetCache, visible_to_tenant};

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
pub async fn execute_job(ctx: &ExecutorContext, claimed: &ClaimedJob) -> MaterializationOutcome {
    execute_job_with_progress(ctx, claimed, |_, _, _| {}).await
}

/// [`execute_job`] with a byte-progress callback (BC-4 / Phase B).
///
/// `on_progress(bytes_done, bytes_expected, upstream_uri)` fires once
/// per ingested/verified path with CUMULATIVE byte counts across the
/// job's whole closure walk (the sum of processed paths' NAR sizes so
/// far). Monotone non-decreasing in `bytes_done`, and
/// `bytes_done <= bytes_expected` at every call — ENFORCED by
/// `MonotoneProgress`, the only constructor of emission sites
/// (bug_159: within-path retry resets used to regress the counter);
/// the final call covers the whole closure. Display-only and
/// droppable: the callback must be cheap and non-blocking (it runs on
/// the walk); the caller forwards it to
/// `ReportMaterializationProgress` fire-and-forget.
// r[impl store.materialize.executor+5]
// r[impl obs.metric.store]
pub async fn execute_job_with_progress(
    ctx: &ExecutorContext,
    claimed: &ClaimedJob,
    on_progress: impl Fn(u64, u64, &str) + Send + Sync + 'static,
) -> MaterializationOutcome {
    let outcome = execute_job_inner(ctx, claimed, on_progress).await;
    // T-6.2 lifecycle counter: one increment per finished execution,
    // labeled by outcome class — the dashboard's execution rates and
    // the upstream-health signal (a rising infra/unobtainable share).
    // Sited at this single chokepoint so every return path of the
    // walk is counted exactly once; the label comes from the ONE
    // alphabet mapping (bug_244).
    metrics::counter!(
        "rio_store_materialization_executions_total",
        "outcome" => outcome_label(outcome.outcome.as_ref())
    )
    .increment(1);
    outcome
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
        // walk itself — but counted like every other outcome so the
        // chokepoint stays total over the alphabet.
        Some(materialization_outcome::Outcome::Aborted(_)) => "aborted",
        // Transient, uncharged retry (merged_bug_178): raced placeholder
        // or upstream 429 — counted so the dashboard sees deferral
        // rates next to the charged classes.
        Some(materialization_outcome::Outcome::RetryLater(_)) => "retry_later",
    }
}

/// The pure clamp law behind `MonotoneProgress` (bug_159): given
/// the previous job-level high-water mark and an absolute candidate
/// report, the emitted pair is `done = max(high_water, done)`,
/// `expected = max(expected, done)` — emitted `done` never regresses
/// and `done <= expected` holds at every call, over ARBITRARY
/// candidate sequences (within-path retry resets are just smaller
/// candidates). Proptest-swept below.
fn clamp_progress(high_water: u64, done: u64, expected: u64) -> (u64, u64) {
    let emit_done = high_water.max(done);
    let emit_expected = expected.max(emit_done);
    (emit_done, emit_expected)
}

/// bug_159: the job-level monotone progress adapter — the ONLY
/// constructor of per-path progress callbacks (the raw job callback is
/// moved in and private, so an unclamped emission site is
/// unwritable). Owns the job's high-water mark and routes EVERY
/// emission (per-path streaming, path-completed ticks) through
/// [`clamp_progress`].
///
/// Why: the documented contract on [`execute_job_with_progress`] is
/// "monotone non-decreasing in `bytes_done`, `bytes_done <=
/// bytes_expected` at every call" — but the per-fetch byte counter is
/// local to each `fetch_nar` attempt, so a stall failover to the next
/// upstream (substitute.rs "download stalled, trying next") or a
/// per-tenant retry restarted `done` at 0 and the pre-fix adapter
/// forwarded `base + done` raw: `bytes_done` regressed below an
/// already-reported value. Monotonicity now lives at the type that
/// owns the contract, not as discipline at each call site.
// r[impl store.materialize.progress-monotone]
struct MonotoneProgress<F: Fn(u64, u64, &str) + Send + Sync + 'static> {
    on_progress: std::sync::Arc<F>,
    /// Job-level emitted-`done` high-water mark. Atomic because the
    /// per-path callbacks are `Fn + Send + Sync` by the substituter's
    /// callback contract; `fetch_max` keeps clamp-and-store one
    /// operation.
    high_water: std::sync::Arc<std::sync::atomic::AtomicU64>,
}

impl<F: Fn(u64, u64, &str) + Send + Sync + 'static> MonotoneProgress<F> {
    fn new(on_progress: F) -> Self {
        MonotoneProgress {
            on_progress: std::sync::Arc::new(on_progress),
            high_water: std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0)),
        }
    }

    /// Clamp an ABSOLUTE candidate report and emit it.
    fn emit(&self, done: u64, expected: u64, uri: &str) {
        let prev = self
            .high_water
            .fetch_max(done, std::sync::atomic::Ordering::SeqCst);
        let (d, e) = clamp_progress(prev, done, expected);
        (self.on_progress)(d, e, uri);
    }

    /// The per-path callback for a path starting at cumulative `base`
    /// bytes (merged_bug_195: `done`/`expected` arrive RELATIVE to the
    /// in-flight path). The only way to build one.
    fn per_path(&self, base: u64) -> impl Fn(u64, u64, &str) + Send + Sync + 'static {
        let this = MonotoneProgress {
            on_progress: std::sync::Arc::clone(&self.on_progress),
            high_water: std::sync::Arc::clone(&self.high_water),
        };
        move |done: u64, expected: u64, uri: &str| {
            this.emit(
                base.saturating_add(done),
                base.saturating_add(expected),
                uri,
            );
        }
    }
}

/// The walk body behind [`execute_job_with_progress`] (split so the
/// outcome counter has a single increment site over every return path).
async fn execute_job_inner(
    ctx: &ExecutorContext,
    claimed: &ClaimedJob,
    on_progress: impl Fn(u64, u64, &str) + Send + Sync + 'static,
) -> MaterializationOutcome {
    // bug_159: every emission goes through the monotone adapter — the
    // raw callback is moved in, so an unclamped site is unwritable.
    // (`SubstProgressFn` is `dyn Fn + 'static`, so the per-path
    // closures own Arc handles, cloned per path.)
    let progress = MonotoneProgress::new(on_progress);

    // ── 1–4. Walk loop with final-verification re-read ───────────────
    // bug_115: the per-job trusted-set memo for the local-visibility
    // probe (two PG queries per tenant, amortized across the walk).
    let mut trust_cache = TrustedSetCache::default();
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
    let mut missing_wanted: Vec<String> = Vec::new();
    let mut missing_references: Vec<String> = Vec::new();
    // merged_bug_005: paths refused on TRUST (present upstream, no
    // verifiable signature under any interested tenant). They ride
    // the same missing cells (the settlement is still Unobtainable —
    // from-source), but the cause string must name the refusal so an
    // operator fixes trusted_keys instead of chasing a phantom miss.
    let mut trust_refused: Vec<String> = Vec::new();
    // BC-4 cumulative progress accounting: bytes of fully-processed
    // paths. The per-path fetch callback adds the in-flight path's
    // streamed bytes on top of `completed_bytes`, with the declared
    // NarSize as the path's expected — so reports are cumulative
    // across the closure, monotone in done, and expected genuinely
    // leads done mid-fetch (merged_bug_195; the narinfo/local-row read
    // precedes the body fetch).
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
        if new_seeds.is_empty() {
            // The final verification pass found no growth: coverage is
            // complete against execution-end live wanted.
            break;
        }

        // Frontier entries carry their CELL: live-wanted seeds vs
        // narinfo reference extensions (merged_bug_193).
        let mut frontier: VecDeque<(String, PathCell)> = new_seeds
            .into_iter()
            .map(|p| (p, PathCell::Wanted))
            .collect();
        while let Some((path, cell)) = frontier.pop_front() {
            if !visited.insert(path.clone()) {
                continue;
            }
            if visited.len() > CLOSURE_WALK_CAP {
                return infra_failure(format!(
                    "closure walk exceeded {CLOSURE_WALK_CAP} paths \
                     (hostile upstream reference chain?)"
                ));
            }
            // bug_042: the local-presence probe is a VERDICT input, so
            // its error PROPAGATES (the pre-fix `.ok().flatten()`
            // mapped a PG blip to "absent" and dragged a locally-
            // present path through upstream substitution — all-404
            // upstreams then produced Unobtainable for a path we
            // already have). `LocalMiss` is the only way to construct
            // a `ConfirmedAbsent` verdict below: upstream absence
            // alone is uncompilable as a missing-path verdict.
            let local_witness = match probe_local(ctx, &path, &tenants, &mut trust_cache).await {
                Ok(LocalPresence::Present(visible, info)) => {
                    // Locally present AND visible to an interested
                    // tenant (bug_115: physical presence alone is NOT
                    // sufficient — the Present arm structurally
                    // requires the visibility witness, so a gate-hidden
                    // row can never be pinned/counted/extended from
                    // here): pin it, count it verified, and extend the
                    // frontier from the LOCAL row's references — the
                    // closure-completeness obligation holds without
                    // touching any upstream.
                    verified_tenants_by_path.insert(path.clone(), visible.tenants().to_vec());
                    if let Err(e) = pin_materialized_path(ctx, claimed, &path).await {
                        return infra_failure(format!("pin-at-ingest failed for {path}: {e}"));
                    }
                    verified.push(path.clone());
                    completed_bytes = completed_bytes.saturating_add(info.nar_size);
                    progress.emit(completed_bytes, completed_bytes, "");
                    for reference in &info.references {
                        let r = reference.as_str().to_string();
                        if r != path && !visited.contains(&r) {
                            frontier.push_back((r, PathCell::Reference));
                        }
                    }
                    continue;
                }
                Ok(LocalPresence::Absent(w)) => w,
                Err(e) => {
                    return infra_failure(format!("local presence probe failed for {path}: {e}"));
                }
            };

            // merged_bug_195: per-path progress through the substitute
            // body fetch — `expected` = completed + the declared
            // NarSize (known before the body), `done` = completed +
            // streamed-so-far, `uri` = the serving upstream.
            // bug_159: minted by the adapter — the only constructor of
            // per-path callbacks; a stall-failover counter reset
            // (substitute.rs retries the next upstream with a fresh
            // byte counter) clamps at the job high-water instead of
            // regressing below an already-reported value.
            let per_path_progress = progress.per_path(completed_bytes);
            // merged_bug_028 / owner Q2 + merged_bug_133: try EVERY
            // interested tenant's upstream view until one serves the
            // path. The loop body ONLY pushes evidence cells (a hit
            // breaks) — ALL failure dispositions exit at the kernel
            // fold below, after every tenant has been consulted, so
            // the deterministic resolve order can never starve a
            // later tenant of its chance to serve (pre-fix: tenant
            // A's charging failure aborted the walk before serving
            // tenant B was tried).
            // Signed Q2: the hit carries its SERVING tenant; S6a's
            // kernel recorder owns the failure cells.
            let mut hit: Option<(Uuid, Box<rio_proto::validated::ValidatedPathInfo>)> = None;
            let mut cells = TenantAttemptCells::new();
            // Per-cell (label, detail) for the outcome message,
            // index-aligned with `cells`.
            let mut cell_msgs: Vec<(&'static str, String)> = Vec::with_capacity(tenants.len());
            for &tenant_id in &tenants {
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
                        // Clean miss under this tenant; the next tenant
                        // may still serve it.
                        cells.record_clean_miss();
                        cell_msgs.push(("", String::new()));
                    }
                    Err(e) => {
                        // merged_bug_178: total classification through
                        // the kernel truth table — no catch-all (a
                        // future SubstituteError variant fails this
                        // match AND the class table).
                        let class = crate::substitute::substitute_error_evidence(&e).0;
                        let (label, retry_after) = match &e {
                            SubstituteError::RateLimited { retry_after } => {
                                ("rate_limited", *retry_after)
                            }
                            SubstituteError::Raced => ("raced", None),
                            _ => ("", None),
                        };
                        let msg = if label.is_empty() {
                            format!("substitution of {path} failed ({class:?}): {e}")
                        } else {
                            format!("substitution of {path}: {e}")
                        };
                        cell_msgs.push((label, msg));
                        // merged_bug_188: the kernel chokepoint owns
                        // the loop-control decision — a Raced verdict
                        // aborts the TENANT axis too (the placeholder
                        // slot is path-keyed; further tenants would
                        // race the same held slot, and a sibling's
                        // charging failure must not dominate the
                        // uncharged race in the fold).
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
                    // Pin-at-ingest (design §5.1): the pin lands BEFORE
                    // the path can appear in any Success report. A pin
                    // failure is an infrastructure failure — reporting
                    // Success for an unpinned path would re-open the
                    // GC-after-vouch window (B2-strong) the pin exists
                    // to close.
                    if let Err(e) = pin_materialized_path(ctx, claimed, &path).await {
                        return infra_failure(format!("pin-at-ingest failed for {path}: {e}"));
                    }
                    ingested.push(path.clone());
                    // Signed Q2: the serving tenant's own-upstream hit
                    // verifies the path for THAT tenant; the remaining
                    // interested tenants are re-checked against the
                    // now-local row (their trust view may accept the
                    // persisted upstream sigs) — verified-for, never
                    // assumed-for.
                    {
                        let mut vt: Vec<Uuid> = vec![serving_tenant];
                        if tenants.len() > 1
                            && let Ok(Some(local)) =
                                crate::metadata::query_path_info(&ctx.pool, &path).await
                        {
                            let signer = ctx.substituter.tenant_signer();
                            for &other in tenants.iter().filter(|t| **t != serving_tenant) {
                                if let Ok(Some(v)) = visible_to_tenant(
                                    &ctx.pool,
                                    signer,
                                    Some(other),
                                    &local,
                                    &mut trust_cache,
                                )
                                .await
                                {
                                    for t in v.tenants() {
                                        if !vt.contains(t) {
                                            vt.push(*t);
                                        }
                                    }
                                }
                            }
                        }
                        verified_tenants_by_path.insert(path.clone(), vt);
                    }
                    // BC-4: the path is fully processed — fold its NAR
                    // size into the cumulative total and fire the
                    // final per-path tick (done == expected for the
                    // completed prefix). Empty upstream URI: the
                    // gateway omits the "from <uri>" suffix when the
                    // field is empty.
                    completed_bytes = completed_bytes.saturating_add(path_info.nar_size);
                    progress.emit(completed_bytes, completed_bytes, "");
                    // Extend the frontier with the narinfo references —
                    // the closure-completeness obligation.
                    for reference in &path_info.references {
                        let r = reference.as_str().to_string();
                        if r != path && !visited.contains(&r) {
                            frontier.push_back((r, PathCell::Reference));
                        }
                    }
                }
                None => match cells.fold() {
                    TenantAttemptsVerdict::ChargeInfra { idx } => {
                        // ≥1 tenant produced charging evidence and no
                        // tenant served: the charge ladder (and park
                        // budget) must see it. The detail names the
                        // first charging tenant's failure.
                        return infra_failure(cell_msgs[idx].1.clone());
                    }
                    TenantAttemptsVerdict::RetryTransient { idx, max } => {
                        // No charge; ≥1 transient: RetryLater so the
                        // scheduler closes UNCHARGED and defers (a 429
                        // wave must never park a healthy job). The
                        // largest Retry-After across tenants rides the
                        // report.
                        let (label, detail) = &cell_msgs[idx];
                        info!(path = %path, class = label,
                              "transient substitute failure; reporting retry-later");
                        return MaterializationOutcome {
                            outcome: Some(materialization_outcome::Outcome::RetryLater(
                                materialization_outcome::RetryLater {
                                    detail: detail.clone(),
                                    retry_after_secs: max.map(|d| d.as_secs()).unwrap_or(0),
                                    class: (*label).to_string(),
                                },
                            )),
                        };
                    }
                    TenantAttemptsVerdict::UntrustedPresent { idx } => {
                        // merged_bug_005: ≥1 tenant found the path
                        // present-but-untrusted and the rest cleanly
                        // missed. Settle toward Unobtainable WITHOUT
                        // the HEAD confirmation — the path IS present
                        // upstream; a sig-blind HEAD 200 proves
                        // nothing about trust and pre-fix converted
                        // this exact state into a permanent
                        // "present but not ingested" infra charge.
                        // The local-miss witness still anchors the
                        // verdict (the path is not locally servable).
                        let _witness: LocalMiss = local_witness;
                        let (_, detail) = &cell_msgs[idx];
                        warn!(path = %path, detail = %detail,
                              "path present upstream but signature-untrusted; \
                               settling unobtainable (uncharged)");
                        trust_refused.push(path.clone());
                        match cell {
                            PathCell::Wanted => missing_wanted.push(path.clone()),
                            PathCell::Reference => missing_references.push(path.clone()),
                        }
                    }
                    TenantAttemptsVerdict::AllCleanMiss => {
                        // Every tenant cleanly missed. The miss verdict
                        // additionally requires the HEAD-probe to
                        // confirm absence under EVERY tenant
                        // (merged_bug_028: any indeterminate or probe
                        // trouble → infra; the local-miss witness from
                        // above completes the proof — bug_042). The
                        // probe loop rides the SAME cells + fold — no
                        // in-loop returns on any tenant axis
                        // (merged_bug_133).
                        let mut probe_cells = TenantAttemptCells::new();
                        let mut probe_msgs: Vec<String> = Vec::with_capacity(tenants.len());
                        for &tenant_id in &tenants {
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
                                    // bug_295: the probe leg rides the
                                    // SAME truth table as the attempt
                                    // leg — congruence per CLASS, not
                                    // per leg (a 429'd probe defers
                                    // uncharged; a 5xx'd probe charges
                                    // exactly like a 5xx'd GET).
                                    // merged_bug_188: and the SAME
                                    // loop control — a raced probe
                                    // aborts the tenant sweep (the
                                    // slot is path-keyed).
                                    probe_msgs.push(format!(
                                        "substitution of {path} hit infrastructure trouble: \
                                         {detail}"
                                    ));
                                    match probe_cells.record_failure(class, retry_after) {
                                        LoopControl::Continue => {}
                                        LoopControl::AbortRaced => break,
                                    }
                                }
                            }
                        }
                        match probe_cells.fold() {
                            TenantAttemptsVerdict::ChargeInfra { idx } => {
                                return infra_failure(probe_msgs[idx].clone());
                            }
                            TenantAttemptsVerdict::RetryTransient { idx, max } => {
                                // No tenant charged; ≥1 probe was
                                // rate-limited: close UNCHARGED and
                                // defer (the probe-leg park-burning
                                // harm case, bug_295).
                                info!(path = %path, class = "rate_limited",
                                      "transient probe failure; reporting retry-later");
                                return MaterializationOutcome {
                                    outcome: Some(materialization_outcome::Outcome::RetryLater(
                                        materialization_outcome::RetryLater {
                                            detail: probe_msgs[idx].clone(),
                                            retry_after_secs: max.map(|d| d.as_secs()).unwrap_or(0),
                                            class: "rate_limited".to_string(),
                                        },
                                    )),
                                };
                            }
                            TenantAttemptsVerdict::UntrustedPresent { .. } => {
                                // Unreachable while the HEAD probe is
                                // sig-blind; a future sig-aware probe's
                                // refusal settles like the attempt
                                // leg's — fall through to the
                                // confirmed-absent settlement with the
                                // trust cause recorded.
                                trust_refused.push(path.clone());
                            }
                            TenantAttemptsVerdict::AllCleanMiss => {}
                        }
                        let _witness: LocalMiss = local_witness;
                        debug!(path = %path, cell = ?cell, tenants = tenants.len(),
                               "path confirmed absent under every interested tenant (and locally)");
                        match cell {
                            PathCell::Wanted => missing_wanted.push(path.clone()),
                            PathCell::Reference => missing_references.push(path.clone()),
                        }
                    }
                },
            }
        }
    }

    // ── 5. Outcome ────────────────────────────────────────────────────
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
        MaterializationOutcome {
            outcome: Some(materialization_outcome::Outcome::Unobtainable(
                materialization_outcome::Unobtainable {
                    cause,
                    missing_paths: missing_wanted,
                    verified_paths: covered,
                    missing_reference_paths: missing_references,
                },
            )),
        }
    }
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
    trust_cache: &mut TrustedSetCache,
) -> Result<LocalPresence, crate::metadata::MetadataError> {
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

    fn make_ctx(pool: PgPool) -> ExecutorContext {
        ExecutorContext {
            substituter: std::sync::Arc::new(
                Substituter::new(pool.clone(), None).with_http_client(sandbox_http()),
            ),
            pool,
        }
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
        let outcome = execute_job(&ctx, &seeded.claimed).await;

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
        let outcome = execute_job(&ctx, &seeded.claimed).await;

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
        let outcome = execute_job(&ctx, &seeded.claimed).await;

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
        let outcome = execute_job(&ctx, &seeded.claimed).await;

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
        let outcome = execute_job(&ctx, &seeded.claimed).await;

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
        let walk = tokio::spawn(async move { execute_job(&ctx, &claimed).await });

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
        let outcome = execute_job(&ctx, &seeded.claimed).await;
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
        let outcome = execute_job(&ctx, &seeded.claimed).await;
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
        let outcome = execute_job(&ctx, &seeded.claimed).await;

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
        let outcome = execute_job(&ctx, &seeded.claimed).await;

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
        let outcome = execute_job(&ctx, &seeded.claimed).await;

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
        let ctx = ExecutorContext {
            substituter: std::sync::Arc::new(
                Substituter::new(db.pool.clone(), None)
                    .with_http_client(sandbox_http())
                    .with_stall_window(std::time::Duration::from_secs(1)),
            ),
            pool: db.pool.clone(),
        };
        let outcome = tokio::time::timeout(
            std::time::Duration::from_secs(30),
            execute_job(&ctx, &seeded.claimed),
        )
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
        let outcome = execute_job(&ctx, &seeded.claimed).await;

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
        let outcome =
            execute_job_with_progress(&ctx, &seeded.claimed, move |done, expected, uri| {
                calls_cb
                    .lock()
                    .unwrap()
                    .push((done, expected, uri.to_string()));
            })
            .await;

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
        let outcome = execute_job(&ctx, &seeded.claimed).await;
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
        let outcome = execute_job(&ctx, &seeded.claimed).await;

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
        let outcome = execute_job(&ctx, &seeded.claimed).await;
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
        let first = execute_job(&ctx, &seeded1.claimed).await;
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
        let outcome = execute_job(&ctx, &seeded2.claimed).await;
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
        let outcome = execute_job(&ctx, &seeded.claimed).await;
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
        let outcome = execute_job(&ctx, &seeded.claimed).await;
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
        let outcome = execute_job(&ctx, &seeded.claimed).await;
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
        let outcome = execute_job(&ctx, &seeded.claimed).await;
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
        let outcome = execute_job(&ctx, &seeded.claimed).await;
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
        let outcome = execute_job(&ctx, &seeded.claimed).await;
        assert!(
            outcome_infra(&outcome).is_some(),
            "an unconfirmable tenant view must report infra, got {outcome:?}"
        );
    }

    // ── bug_159: monotone progress ────────────────────────────────────

    // r[verify store.materialize.progress-monotone]
    /// bug_159: the stall-failover regression trace. Path A completes
    /// at 100 cumulative; path B (base 100) streams to 120 relative,
    /// the download stalls, and the failover attempt restarts the
    /// per-fetch counter at 0 — the pre-fix adapter forwarded
    /// base+done raw and bytes_done regressed below the
    /// already-reported value. RED (strawman: clamp bypassed, raw
    /// forward): the emitted sequence carried (110, 210) after
    /// (220, 300) — assert_eq! sequence mismatch at the reset event.
    #[test]
    fn monotone_progress_clamps_stall_failover_resets() {
        use std::sync::{Arc, Mutex};
        let got: Arc<Mutex<Vec<(u64, u64)>>> = Arc::default();
        let sink = Arc::clone(&got);
        let progress = MonotoneProgress::new(move |d: u64, e: u64, _u: &str| {
            sink.lock().unwrap().push((d, e));
        });

        progress.emit(100, 100, ""); // path A fully processed
        let cb = progress.per_path(100); // path B starts at base=100
        cb(120, 200, "u1"); // attempt 1 streams 120 of 200
        cb(10, 200, "u2"); // stall failover: counter RESET to 10
        cb(180, 200, "u2"); // attempt 2 catches up past the mark
        progress.emit(300, 300, ""); // path B fully processed

        let events = got.lock().unwrap().clone();
        assert_eq!(
            events,
            vec![(100, 100), (220, 300), (220, 300), (280, 300), (300, 300)],
            "reset clamps at the high-water; catch-up resumes"
        );
        let mut last = 0u64;
        for (d, e) in events {
            assert!(d >= last, "emitted done regressed: {d} after {last}");
            assert!(d <= e, "done {d} > expected {e}");
            last = d;
        }
    }

    proptest::proptest! {
        // r[verify store.materialize.progress-monotone]
        /// The pure clamp law over ARBITRARY candidate sequences
        /// (within-path resets are just smaller candidates): emitted
        /// done is non-decreasing and done <= expected at every step.
        #[test]
        fn clamp_progress_monotone_over_arbitrary_traces(
            events in proptest::collection::vec((0u64..1_000_000, 0u64..1_000_000), 1..100)
        ) {
            let mut hw = 0u64;
            let mut last = 0u64;
            for (done, expected) in events {
                let (d, e) = clamp_progress(hw, done, expected);
                hw = hw.max(d);
                proptest::prop_assert!(d >= last, "regressed: {} after {}", d, last);
                proptest::prop_assert!(d <= e, "done {} > expected {}", d, e);
                last = d;
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
        let outcome = execute_job(&ctx, &seeded.claimed).await;
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
        let outcome = execute_job(&ctx, &seeded.claimed).await;
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
        let outcome = execute_job(&ctx, &seeded.claimed).await;
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
}
