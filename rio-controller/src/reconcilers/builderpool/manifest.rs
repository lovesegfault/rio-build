//! Manifest-mode BuilderPool: per-derivation-sized Job spawning.
//!
//! When `BuilderPoolSpec.sizing == Manifest`, the reconciler polls
//! `AdminService.GetCapacityManifest` (detailed shape behind
//! `ClusterStatus.queued_derivations` — one `DerivationResourceEstimate`
//! per queued-ready derivation, bucketed scheduler-side per ADR-020
//! § Decision ¶2) and spawns Jobs with `ResourceRequirements` FROM the
//! manifest bucket instead of FROM `spec.resources`.
//!
//! Versus [`ephemeral`](super::ephemeral): same Job-spawn machinery,
//! different pod-spec source. `reconcile_ephemeral` reads the scalar
//! `queued_derivations` and spawns N identical Jobs sized by
//! `spec.resources`. `reconcile_manifest` reads the per-derivation
//! breakdown and spawns heterogeneous Jobs — a queue with one 4Gi
//! build and one 48Gi build gets one small pod and one large pod,
//! not two medium pods.
//!
//! # Diff algorithm
//!
//!   1. **Poll** `GetCapacityManifest` + `ClusterStatus` (for
//!      cold-start count).
//!   2. **Group** estimates by `(est_memory_bytes, est_cpu_millicores)`
//!      → `BTreeMap<Bucket, count>`. BTreeMap for deterministic
//!      iteration (stable Job naming across reconcile ticks; operators
//!      see consistent ordering in `kubectl get jobs`).
//!   3. **Inventory** live Jobs: label selector
//!      `rio.build/pool={name},rio.build/sizing=manifest`, extract
//!      each Job's bucket from `rio.build/memory-class` +
//!      `rio.build/cpu-class` labels → same-shaped map.
//!   4. **Diff UP**: per bucket, `deficit = demand.saturating_sub(
//!      supply)`. Over-provisioned (supply > demand) → zero spawns.
//!   5. **Diff DOWN**: per bucket, `surplus = supply.saturating_sub(
//!      demand)`. A bucket surplus for `scale_down_window` (600s
//!      default, same as STS autoscaler anti-flap) → delete surplus
//!      Jobs. Skips Jobs whose pods are mid-build (`running_builds
//!      > 0` via `ListExecutors`).
//!   6. **Cold-start floor**: `queued_derivations - manifest.len()`
//!      derivations have no `build_history` sample → manifest omits
//!      them (proto doc at `admin_types.proto:249`). Spawn those at
//!      `spec.resources` (the operator-configured floor).
//!   7. **Spawn**: per deficit, `build_pod_spec(..., Some(resources))`.
//!      Label `rio.build/memory-class={n}Gi`, `rio.build/cpu-class={n}m`.
//!      Name `{pool}-mf-{mem}g-{cpu}m-{random6}`.
//!
//! # Inventory label round-trip
//!
//! THE critical invariant: the label values [`bucket_labels`] SETS
//! must exactly match what [`parse_bucket_from_labels`] READS. A
//! typo = perpetual over-spawn (every reconcile thinks supply=0).
//! `label_roundtrip` in tests proves they agree.
//!
//! # Not `RIO_EPHEMERAL=1`
//!
//! Manifest pods are long-lived (ADR-020 § Decision ¶4): they loop
//! for MULTIPLE builds of the same size class. A 48Gi pod is expensive
//! to schedule (node provision, image pull, FUSE warm); amortizing
//! that over several 48Gi builds is the point. Ephemeral mode's
//! "exit after one build" is for ISOLATION (untrusted tenants);
//! manifest mode is for RIGHT-SIZING (heterogeneous workloads).

use std::collections::BTreeMap;
use std::time::{Duration, Instant};

use k8s_openapi::api::batch::v1::{Job, JobSpec};
use k8s_openapi::api::core::v1::{PodTemplateSpec, ResourceRequirements};
use k8s_openapi::apimachinery::pkg::api::resource::Quantity;
use k8s_openapi::apimachinery::pkg::apis::meta::v1::OwnerReference;
use kube::ResourceExt;
use kube::api::{DeleteParams, ListParams, ObjectMeta};
use kube::runtime::controller::Action;
use rio_proto::types::{
    DerivationResourceEstimate, ExecutorInfo, GetCapacityManifestRequest, ListExecutorsRequest,
};
use tracing::{debug, info, warn};

use crate::crds::builderpool::BuilderPool;
use crate::error::{Error, Result};
use crate::reconcilers::Ctx;

use super::POOL_LABEL;
use super::builders::{self, SchedulerAddrs};
/// Re-export: `Bucket` now lives in [`super::job_common`] so
/// `reconcilers::mod` can see the same alias `pub(crate)`. Re-
/// exported here so `manifest_tests.rs`'s `use ...::manifest::Bucket`
/// stays intact.
pub(super) use super::job_common::Bucket;
use super::job_common::{
    SpawnOutcome, is_active_job, is_failed_job, job_reconcile_prologue, patch_job_pool_status,
    random_suffix, spawn_prerequisites, try_spawn_job,
};

/// Requeue interval. Same as ephemeral (~10s) — manifest is demand-
/// driven, not drift-driven. A derivation queued between ticks waits
/// one interval before a pod is spawned for it. See ephemeral.rs's
/// `EPHEMERAL_REQUEUE` doc for the latency-vs-apiserver-load tradeoff.
const MANIFEST_REQUEUE: Duration = Duration::from_secs(10);

/// Manifest sizing mode label. Distinguishes manifest Jobs from
/// ephemeral Jobs when both are running for the same pool (edge case:
/// operator flips `spec.sizing` between reconciles). The inventory
/// query filters on this so it doesn't count ephemeral Jobs as
/// manifest supply.
pub(super) const SIZING_LABEL: &str = "rio.build/sizing";
pub(super) const SIZING_MANIFEST: &str = "manifest";

/// Per-bucket labels. The inventory reads these to reconstruct the
/// bucket map. The VALUE format (`{n}Gi`, `{n}m`) is what operators
/// type in `kubectl get job -l rio.build/memory-class=48Gi` — human-
/// readable, not raw bytes.
pub(super) const MEMORY_CLASS_LABEL: &str = "rio.build/memory-class";
pub(super) const CPU_CLASS_LABEL: &str = "rio.build/cpu-class";

/// Reserved label value for cold-start Jobs (manifest omitted the
/// derivation → spawned at `spec.resources` floor). Cold-start Jobs
/// NEED a memory-class/cpu-class label (the inventory selector matches
/// on `rio.build/sizing=manifest` alone; a Job without the class
/// labels would still match but [`parse_bucket_from_labels`] returns
/// None → not counted → double-spawn on next tick). `"floor"` is a
/// stable sentinel.
pub(super) const FLOOR_CLASS: &str = "floor";

/// Floor for per-tick Failed-Job deletes. The actual cap is
/// `max(FAILED_SWEEP_MIN, spec.replicas.max)` — under a full crash-
/// loop (bad image, `backoff_limit=0`), the spawn pass fires
/// `headroom` replacements every tick, bounded by `replicas.max`.
/// The sweep MUST clear at least that many to converge (net ≤
/// 0/tick). This floor guarantees small pools (`max < 20`) still
/// sweep at a reasonable rate and clear historical backlog.
/// Operators wanting instant clear: `kubectl delete jobs -l
/// rio.build/sizing=manifest --field-selector status.successful=0`.
pub(super) const FAILED_SWEEP_MIN: usize = 20;

/// Per-tick Failed-Job sweep cap. Tracks `replicas.max` so the sweep
/// converges under full crash-loop (net accumulation ≤ 0 per tick:
/// at most `replicas.max` Failed Jobs spawn, at most that many swept).
/// Floors at FAILED_SWEEP_MIN for small pools — even replicas.max=2
/// gets a 20/tick sweep so a short burst clears quickly.
///
/// The `.max(0)` clamp: `replicas.max` is i32 (k8s typed
/// `IntOrString` backing type); negative values have no CEL floor in
/// manifest mode. `-1_i32 as usize` wraps to `usize::MAX`.
// r[impl ctrl.pool.manifest-failed-sweep+2]
pub(super) fn sweep_cap(replicas_max: i32) -> usize {
    FAILED_SWEEP_MIN.max(replicas_max.max(0) as usize)
}

/// Emit `CrashLoopDetected` Warning when Failed-Job count crosses
/// this. 3 Failed Jobs from a pool with `backoff_limit=0` is 3
/// consecutive pod crashes — strong crash-loop signal, not a
/// transient one-off.
pub(super) const CRASH_LOOP_WARN_THRESHOLD: usize = 3;

/// K8s Event reason for manifest crash-loop detection (Failed Jobs
/// accumulating under `backoff_limit=0`).
const REASON_CRASH_LOOP: &str = "CrashLoopDetected";

/// One spawn directive from [`compute_spawn_plan`]. `bucket: None` →
/// cold-start (use `spec.resources` floor). `count: 0` is never
/// emitted (filtered).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct SpawnDirective {
    pub bucket: Option<Bucket>,
    pub count: usize,
}

// r[impl ctrl.pool.manifest-reconcile]
/// Reconcile a manifest-mode BuilderPool: poll manifest, diff against
/// inventory, spawn Jobs for the deficit.
///
/// NO StatefulSet / headless Service / PDB — same as ephemeral
/// (Job-based). DIFFERENCE from ephemeral: no `RIO_EPHEMERAL=1`
/// (pods loop for multiple builds); per-bucket `ResourceRequirements`
/// (not one-size from `spec.resources`).
pub(super) async fn reconcile_manifest(wp: &BuilderPool, ctx: &Ctx) -> Result<Action> {
    let (ns, name, jobs_api) = job_reconcile_prologue(wp, ctx)?;

    // ---- Poll: manifest + ClusterStatus ----
    // Both RPCs, same fail-open behavior as ephemeral: RPC down →
    // treat as empty → no spawn → requeue. SchedulerUnreachable
    // condition on the status so operators see WHY. Cloning the
    // client twice (tonic is Arc-internal).
    let mut admin = ctx.admin.clone();
    let (estimates, queued_total, scheduler_err): (
        Vec<DerivationResourceEstimate>,
        u32,
        Option<String>,
    ) = match (
        admin
            .get_capacity_manifest(GetCapacityManifestRequest {})
            .await,
        ctx.admin.clone().cluster_status(()).await,
    ) {
        (Ok(manifest), Ok(status)) => (
            manifest.into_inner().estimates,
            status.into_inner().queued_derivations,
            None,
        ),
        // Either RPC down → fail open. The manifest alone isn't
        // enough (missing cold-start count); ClusterStatus alone
        // isn't enough (missing buckets). Both-or-nothing.
        (Err(e), _) | (_, Err(e)) => {
            warn!(
                pool = %name, error = %e,
                "scheduler poll failed; treating as empty manifest, will retry"
            );
            (Vec::new(), 0, Some(e.to_string()))
        }
    };

    // ---- Group: manifest → demand BTreeMap ----
    let demand = group_by_bucket(&estimates);
    // Cold-start count: derivations the manifest OMITS (no
    // build_history sample, admin_types.proto:249). Both RPCs walk
    // the same ready_queue so `queued_total >= manifest.len()`
    // structurally — saturating anyway for the RPC-race edge case
    // (manifest taken at tick N, ClusterStatus at tick N+1 after
    // a dequeue).
    let cold_start = (queued_total as usize).saturating_sub(estimates.len());

    // ---- Inventory: live Jobs → supply BTreeMap ----
    // Label selector includes BOTH pool AND sizing=manifest. Without
    // the sizing filter, an ephemeral Job for the same pool (operator
    // flipped sizing mid-run) would count as supply.
    let selector = format!("{POOL_LABEL}={name},{SIZING_LABEL}={SIZING_MANIFEST}");
    let jobs = jobs_api
        .list(&ListParams::default().labels(&selector))
        .await?;
    // "Active" = same definition as ephemeral (not Complete, not
    // Failed). A Failed-but-unreap'd Job is NOT supply — we want a
    // replacement. This is why we don't count by label presence
    // alone; status matters.
    let active_jobs: Vec<&Job> = jobs.items.iter().filter(|j| is_active_job(j)).collect();
    // Failed Jobs: not supply, not capacity, but still ours to reap.
    // backoff_limit=0 means one pod crash → Job Failed permanently.
    // Under crash-loop (bad image, OOM-on-start) these accumulate at
    // up to `replicas.max` per tick (headroom-worth all fail); sweep
    // them alongside idle-surplus deletes. Cap tracks the pool's own
    // spawn ceiling — see FAILED_SWEEP_MIN. failed_total is the
    // pre-cap count for the Warning event; the sweep acts on the
    // capped slice.
    let failed_total = jobs.items.iter().filter(|j| is_failed_job(j)).count();
    let cap = sweep_cap(wp.spec.replicas.max);
    let failed_jobs = select_failed_jobs(&jobs.items, cap);
    let supply = inventory_by_bucket(&active_jobs);
    let cold_start_supply = active_jobs.iter().filter(|j| is_floor_job(j)).count();
    let active_total: i32 = active_jobs.len().try_into().unwrap_or(i32::MAX);

    // ---- Sweep Failed Jobs FIRST ----
    // This MUST run before spawn: under a namespace ResourceQuota on
    // count/jobs.batch (GKE Autopilot default, common in hardened
    // clusters), a crash-loop fills the quota with Failed Jobs. If
    // spawn-before-sweep, jobs_api.create 403s on quota exhaustion →
    // return Err → sweep never runs → deadlock (can't clear quota to
    // make room for the spawn that would succeed next). Sweep-first
    // clears dead weight; spawn then has room.
    //
    // Separate from the idle-reapable pass below: Failed Jobs need no
    // idle-check (no running pod) and no ListExecutors RPC. This block
    // is self-contained — runs unconditionally, bounded-per-tick
    // (select_failed_jobs caps internally at cap =
    // sweep_cap(replicas.max)).
    //
    // CrashLoopDetected: operator visibility via `kubectl describe
    // builderpool`. The message interpolates a coarse tier
    // (crash_loop_tier), not the exact count — K8s deduplicates
    // events by (reason, message), so a stable message lets the
    // apiserver collapse per-tick emits into one event with a
    // rising .count. Exact count would change every tick → no dedup
    // → event flood compounding the Job flood.
    if failed_total >= CRASH_LOOP_WARN_THRESHOLD {
        use kube::runtime::events::{Event as KubeEvent, EventType};
        ctx.publish_event(
            wp,
            &KubeEvent {
                type_: EventType::Warning,
                reason: REASON_CRASH_LOOP.into(),
                note: Some(format!(
                    "{} Failed manifest Jobs (backoff_limit=0); check \
                     pod logs for crash cause. Sweeping up to {} per \
                     tick. To clear immediately: kubectl delete jobs \
                     -l {SIZING_LABEL}={SIZING_MANIFEST} \
                     --field-selector status.successful=0",
                    crash_loop_tier(failed_total),
                    cap,
                )),
                action: "Sweep".into(),
                secondary: None,
            },
        )
        .await;
    }
    for job in &failed_jobs {
        let job_name = job.metadata.name.as_deref().unwrap_or("<unnamed>");
        match jobs_api.delete(job_name, &DeleteParams::default()).await {
            Ok(_) => {
                info!(
                    pool = %name, job = %job_name,
                    "swept Failed manifest Job (backoff_limit=0 crash)"
                );
            }
            Err(kube::Error::Api(ae)) if ae.code == 404 => {
                debug!(pool = %name, job = %job_name, "Failed Job already deleted");
            }
            Err(e) => {
                warn!(
                    pool = %name, job = %job_name, error = %e,
                    "failed to sweep Failed Job; will retry next tick"
                );
            }
        }
    }

    // ---- Diff: spawn (scale-up) ----
    let plan = compute_spawn_plan(&demand, &supply, cold_start, cold_start_supply);
    let to_spawn: usize = plan.iter().map(|d| d.count).sum();

    // ---- Diff: surplus (scale-down) ----
    // Per-bucket idle grace. A bucket surplus for `scale_down_window`
    // → eligible for delete. State lives across reconcile ticks in
    // Ctx (keyed by `{ns}/{name}` — same pool across ticks, same
    // idle clock). Lock held for one map update.
    let surplus = compute_surplus(&demand, &supply);
    let pool_key = format!("{ns}/{name}");
    let reapable: BTreeMap<Bucket, usize> = {
        let mut state = ctx.manifest_idle.lock().expect("manifest_idle poisoned");
        let idle = state.entry(pool_key).or_default();
        update_idle_and_reapable(idle, &surplus, Instant::now(), ctx.scale_down_window)
    };

    // ---- Spawn ----
    // Ceiling: same `spec.replicas.max` cap as ephemeral. A manifest
    // with 200 distinct derivations shouldn't spawn 200 pods if the
    // operator said max=10. `truncate_plan` applies per-bucket-floor:
    // every bucket with demand gets ≥1 before any gets 2 — prevents
    // the small-first starvation livelock where large buckets and
    // cold-start never spawn under sustained tiny-heavy load. Spec:
    // `ctrl.pool.manifest-fairness` in docs/src/components/controller.md.
    let ceiling = wp.spec.replicas.max;
    let headroom = ceiling.saturating_sub(active_total).max(0) as usize;
    let budget = to_spawn.min(headroom);
    let truncated = truncate_plan(&plan, budget);

    if !truncated.is_empty() {
        let (oref, scheduler) = spawn_prerequisites(wp, ctx)?;

        for directive in &truncated {
            for _ in 0..directive.count {
                let job = build_manifest_job(
                    wp,
                    oref.clone(),
                    &scheduler,
                    &ctx.store_addr,
                    directive.bucket,
                )?;
                let job_name = job
                    .metadata
                    .name
                    .clone()
                    .ok_or_else(|| Error::InvalidSpec("job name missing".into()))?;
                match try_spawn_job(&jobs_api, &job).await {
                    SpawnOutcome::Spawned => {
                        info!(
                            pool = %name, job = %job_name,
                            bucket = ?directive.bucket,
                            "spawned manifest Job"
                        );
                    }
                    SpawnOutcome::NameCollision => {
                        debug!(pool = %name, job = %job_name, "Job name collision; will retry");
                    }
                    SpawnOutcome::Failed(e) => {
                        // warn+continue (P0516): a spawn failure
                        // (quota, webhook rejection, transient apiserver
                        // blip) shouldn't skip the rest of this tick's
                        // work — subsequent spawns in the batch may
                        // succeed (different bucket → different resource
                        // limits), and the idle-reapable pass below is
                        // independent. Matches delete-error handling in
                        // the sweep loop.
                        //
                        // TODO(P0522): N-consecutive-fail threshold +
                        // metric HERE. Prefer caller-side (not inside
                        // try_spawn_job) so per-reconciler thresholds
                        // can differ; keeps try_spawn_job stateless.
                        warn!(
                            pool = %name, job = %job_name,
                            bucket = ?directive.bucket, error = %e,
                            "manifest Job spawn failed; continuing tick"
                        );
                    }
                }
            }
        }
    } else {
        debug!(
            pool = %name, demand = ?demand, supply = ?supply,
            cold_start, active_total, ceiling,
            "no manifest Jobs to spawn"
        );
    }

    // ---- Scale-down: delete reapable Jobs ----
    // Only poll ListExecutors if there's actually something to reap
    // — common case is empty (nothing surplus long enough). Saves
    // an RPC per tick on the hot path.
    if !reapable.is_empty() {
        // Fail-open: RPC down → can't verify idle → delete nothing.
        // Scale-up (above) is unaffected. Same fail-open philosophy
        // as the poll phase — a transient scheduler blip shouldn't
        // orphan builds.
        //
        // status_filter: "alive" excludes draining/dead. A draining
        // executor might have running_builds > 0 (finishing what it
        // has) — we want those visible. But a DEAD executor (hasn't
        // heartbeat in timeout) with running_builds > 0 is stale
        // data. Actually "" (no filter) is safest: we match by pod-
        // name prefix, and a dead executor's Job is either already
        // Failed (filtered from active_jobs) or about to be. An
        // extra-conservative skip costs one more tick, not a bug.
        match ctx
            .admin
            .clone()
            .list_executors(ListExecutorsRequest {
                status_filter: String::new(),
            })
            .await
        {
            Ok(resp) => {
                let executors = resp.into_inner().executors;
                let deletable = select_deletable_jobs(&active_jobs, &reapable, &executors);
                for job in deletable {
                    // name is Some (select_deletable_jobs skips None).
                    let job_name = job.metadata.name.as_deref().unwrap_or("<unnamed>");
                    // Background propagation: K8s deletes the Job's
                    // pod asynchronously. Pod gets SIGTERM → worker's
                    // drain handler (acquire_many on build semaphore)
                    // exits cleanly. We've already verified
                    // running_builds == 0, so the semaphore is
                    // immediately acquirable.
                    match jobs_api.delete(job_name, &DeleteParams::default()).await {
                        Ok(_) => {
                            info!(
                                pool = %name, job = %job_name,
                                bucket = ?parse_bucket_from_labels(job),
                                "deleted surplus manifest Job (idle grace elapsed)"
                            );
                        }
                        Err(kube::Error::Api(ae)) if ae.code == 404 => {
                            // Already gone (another reconcile tick
                            // raced us, or ownerRef GC). Fine.
                            debug!(pool = %name, job = %job_name, "Job already deleted");
                        }
                        Err(e) => {
                            // Don't abort the whole reconcile on one
                            // delete failure — log and continue. Next
                            // tick retries (Job is still surplus).
                            warn!(
                                pool = %name, job = %job_name, error = %e,
                                "failed to delete surplus manifest Job; will retry next tick"
                            );
                        }
                    }
                }
            }
            Err(e) => {
                warn!(
                    pool = %name, error = %e, reapable = ?reapable.keys().collect::<Vec<_>>(),
                    "ListExecutors failed; skipping scale-down this tick (can't verify idle)"
                );
            }
        }
    }

    // ---- Status patch ----
    // `replicas` = active Jobs, `desired` = ceiling. SchedulerUnreachable
    // condition reflects BOTH RPCs (either down → True).
    patch_job_pool_status(
        ctx,
        wp,
        &ns,
        &name,
        active_total,
        active_total,
        ceiling,
        scheduler_err.as_deref(),
    )
    .await?;

    Ok(Action::requeue(MANIFEST_REQUEUE))
}

/// Group manifest estimates by `(memory, cpu)` bucket. The scheduler
/// pre-bucketed (rounded to 4GiB/2000m grid) so two derivations with
/// 7.8GiB and 7.9GiB EMAs both land at the 8GiB key — no f64 here.
pub(super) fn group_by_bucket(estimates: &[DerivationResourceEstimate]) -> BTreeMap<Bucket, usize> {
    let mut m = BTreeMap::new();
    for e in estimates {
        *m.entry((e.est_memory_bytes, e.est_cpu_millicores))
            .or_insert(0) += 1;
    }
    m
}

/// Reconstruct the supply map from live Jobs' labels. Skips Jobs
/// whose labels don't parse (wouldn't happen with Jobs WE spawned,
/// but a foreign Job with `rio.build/sizing=manifest` and garbage
/// class labels shouldn't crash the reconciler — just don't count
/// it). Cold-start (`floor`-labeled) Jobs are counted SEPARATELY
/// by the caller; they return `None` here.
pub(super) fn inventory_by_bucket(jobs: &[&Job]) -> BTreeMap<Bucket, usize> {
    let mut m = BTreeMap::new();
    for j in jobs {
        if let Some(bucket) = parse_bucket_from_labels(j) {
            *m.entry(bucket).or_insert(0) += 1;
        }
    }
    m
}

/// Is this a cold-start Job (labeled `memory-class=floor`)?
/// Separate from [`parse_bucket_from_labels`] (which returns None for
/// floor) so the caller can count floor supply without a second
/// label parse.
fn is_floor_job(j: &Job) -> bool {
    j.metadata
        .labels
        .as_ref()
        .and_then(|l| l.get(MEMORY_CLASS_LABEL))
        .map(|v| v == FLOOR_CLASS)
        .unwrap_or(false)
}

/// Parse `(memory_bytes, cpu_millicores)` from a Job's class labels.
///
/// Inverse of [`bucket_labels`]: `"8Gi"` → `8 * 1024^3`, `"2000m"` →
/// `2000`. Returns `None` if either label is missing, malformed, or
/// the reserved `"floor"` sentinel.
///
/// NOT using K8s `Quantity::parse` — overkill. Our labels are always
/// `{int}Gi` / `{int}m` (we're the only writer); a `strip_suffix` +
/// `parse::<u64>` is tight and the parse failure is a real signal
/// (someone hand-edited a label, or a unit-change bug).
pub(super) fn parse_bucket_from_labels(j: &Job) -> Option<Bucket> {
    let labels = j.metadata.labels.as_ref()?;
    let mem_str = labels.get(MEMORY_CLASS_LABEL)?;
    let cpu_str = labels.get(CPU_CLASS_LABEL)?;
    // `floor` → None (cold-start Jobs aren't a bucket, they're the
    // absence of one). Caller counts them separately.
    if mem_str == FLOOR_CLASS || cpu_str == FLOOR_CLASS {
        return None;
    }
    let mem_gi: u64 = mem_str.strip_suffix("Gi")?.parse().ok()?;
    let cpu_m: u32 = cpu_str.strip_suffix('m')?.parse().ok()?;
    Some((mem_gi * GI, cpu_m))
}

/// 1 GiB in bytes. Manifest buckets are in bytes (proto field);
/// labels are in Gi (operator-readable). Both conversions go
/// through this constant so they can't drift.
const GI: u64 = 1024 * 1024 * 1024;

// r[impl ctrl.pool.manifest-labels]
/// Label values for a bucket. `None` → cold-start floor.
///
/// Memory: bytes → Gi (integer; scheduler rounds to 4GiB boundaries
/// so this is always exact — a non-4GiB-aligned bucket would truncate,
/// but that's a scheduler-side bug). CPU: millicores verbatim.
///
/// The FORMAT is load-bearing: [`parse_bucket_from_labels`] is the
/// inverse. Change one without the other → round-trip breaks →
/// perpetual over-spawn. `label_roundtrip` test catches it.
pub(super) fn bucket_labels(bucket: Option<Bucket>) -> (String, String) {
    match bucket {
        Some((mem_bytes, cpu_m)) => (format!("{}Gi", mem_bytes / GI), format!("{cpu_m}m")),
        None => (FLOOR_CLASS.into(), FLOOR_CLASS.into()),
    }
}

/// Compute the spawn plan: per-bucket deficit + cold-start deficit.
///
/// Pure — no K8s, no RPC. T4's unit-test target.
///
/// For each bucket in `demand`: spawn `demand - supply` (saturating).
/// A bucket in `supply` but NOT in `demand` (over-provisioned, or
/// the derivation completed between ticks) → zero spawns for that
/// bucket (no negative spawn; scale-down is [`compute_surplus`]'s
/// job, applied separately in the reconcile loop).
///
/// Cold-start: `cold_start_demand - cold_start_supply` Jobs at `None`
/// (→ `spec.resources` floor). Same subtraction as ephemeral's
/// `spawn_count`: a floor Job already in flight claims one cold-start
/// derivation.
///
/// Returns only non-zero directives (no `count: 0` noise).
pub(super) fn compute_spawn_plan(
    demand: &BTreeMap<Bucket, usize>,
    supply: &BTreeMap<Bucket, usize>,
    cold_start_demand: usize,
    cold_start_supply: usize,
) -> Vec<SpawnDirective> {
    let mut plan = Vec::new();
    // BTreeMap iteration: deterministic (by key). Smaller buckets
    // first — under extreme budget pressure (budget < num_buckets),
    // the floor pass covers small buckets first. But every bucket
    // that fits gets its floor slot; no bucket starves. See
    // `truncate_plan`.
    for (&bucket, &want) in demand {
        let have = supply.get(&bucket).copied().unwrap_or(0);
        let deficit = want.saturating_sub(have);
        if deficit > 0 {
            plan.push(SpawnDirective {
                bucket: Some(bucket),
                count: deficit,
            });
        }
    }
    let cold_deficit = cold_start_demand.saturating_sub(cold_start_supply);
    if cold_deficit > 0 {
        plan.push(SpawnDirective {
            bucket: None,
            count: cold_deficit,
        });
    }
    plan
}

// r[impl ctrl.pool.manifest-fairness]
/// Truncate a spawn plan at `budget`, guaranteeing per-bucket-floor:
/// every directive with `count > 0` gets at least 1 before any gets 2.
///
/// Two passes. Pass 1 (floor): iterate `plan`, allocate 1 to each
/// directive until budget exhausted. Pass 2 (proportional): distribute
/// remaining budget proportional to `directive.count` via integer
/// largest-remainder (no f64).
///
/// [`compute_spawn_plan`] emits BTreeMap-ordered (small-first) +
/// cold-start-last. Pass 1 preserves that order for the floor slot, so
/// under extreme budget (budget < num_buckets) small buckets still win —
/// but every bucket that fits in the budget gets its one. Under
/// sustained load, N ticks where N = plan.len() guarantees full
/// coverage.
///
/// `budget` is capped at total demand (`Σ plan[i].count`): if the
/// operator grants more headroom than the queue needs, we spawn
/// exactly demand, not `budget`. The old inline loop got this for
/// free by iterating up to `directive.count`; two-pass needs it
/// explicit.
///
/// Returns directives with `count > 0` only (same filter as
/// [`compute_spawn_plan`]).
pub(super) fn truncate_plan(plan: &[SpawnDirective], budget: usize) -> Vec<SpawnDirective> {
    if budget == 0 || plan.is_empty() {
        return Vec::new();
    }
    let total_demand: usize = plan.iter().map(|d| d.count).sum();
    let budget = budget.min(total_demand);

    let mut out: Vec<SpawnDirective> = plan
        .iter()
        .map(|d| SpawnDirective {
            bucket: d.bucket,
            count: 0,
        })
        .collect();
    let mut remaining = budget;

    // Pass 1: floor — one each, in plan order.
    for (i, d) in plan.iter().enumerate() {
        if remaining == 0 {
            break;
        }
        if d.count > 0 {
            out[i].count = 1;
            remaining -= 1;
        }
    }

    // Pass 2: proportional on what's left. Largest-remainder: integer
    // proportion first, then hand out the residue one at a time to the
    // directives with the largest fractional part. Stable: ties break
    // by plan-order (small-first).
    if remaining > 0 {
        let total_want: usize = plan.iter().map(|d| d.count.saturating_sub(1)).sum();
        if total_want > 0 {
            let mut residues: Vec<(usize, u64)> = Vec::with_capacity(plan.len());
            let mut distributed = 0usize;
            for (i, d) in plan.iter().enumerate() {
                let want = d.count.saturating_sub(1);
                // Scaled by u64 to avoid f64 imprecision on the remainder.
                let share_num = (want as u64) * (remaining as u64);
                let share = (share_num / total_want as u64) as usize;
                let residue = share_num % total_want as u64;
                out[i].count += share;
                distributed += share;
                residues.push((i, residue));
            }
            // Hand out the residue (remaining - distributed), largest-
            // remainder first. Stable sort preserves plan-order on ties.
            residues.sort_by(|a, b| b.1.cmp(&a.1));
            for (i, _) in residues.into_iter().take(remaining - distributed) {
                out[i].count += 1;
            }
        }
        // total_want == 0 means every directive wanted exactly 1 and
        // pass 1 covered them all. Residue budget is unused (no more
        // demand). Correct — we don't over-spawn.
    }

    out.into_iter().filter(|d| d.count > 0).collect()
}

/// Compute per-bucket surplus: `supply - demand` where positive.
///
/// Mirror of [`compute_spawn_plan`] for the scale-DOWN direction.
/// Iterates `supply` (not `demand`) — a bucket in supply but absent
/// from demand is by definition surplus with `demand = 0` (the
/// derivation completed, or the queue drained). That's the PRIMARY
/// scale-down case: work finished, pod idles.
///
/// Cold-start (floor) is NOT tracked here. Floor Jobs are
/// one-shot-ish (cold-start derivations get `build_history` samples
/// on first run → move to regular buckets → floor demand naturally
/// trends to 0). Tracking floor idle would also require
/// `Option<Bucket>` keys throughout; not worth the API bloat when
/// the floor set self-shrinks.
///
/// Pure — no K8s, no clock. T4's unit-test target.
pub(super) fn compute_surplus(
    demand: &BTreeMap<Bucket, usize>,
    supply: &BTreeMap<Bucket, usize>,
) -> BTreeMap<Bucket, usize> {
    let mut out = BTreeMap::new();
    for (&bucket, &have) in supply {
        let want = demand.get(&bucket).copied().unwrap_or(0);
        let surplus = have.saturating_sub(want);
        if surplus > 0 {
            out.insert(bucket, surplus);
        }
    }
    out
}

// r[impl ctrl.pool.manifest-scaledown]
/// Update per-bucket idle timestamps and return buckets eligible
/// for scale-down.
///
/// Three-phase:
///   1. **Prune**: drop `idle_since` entries for buckets no longer
///      surplus. Demand returned (or all those Jobs died) → reset
///      the clock. Next time they go surplus, the window restarts.
///   2. **Record**: for each currently-surplus bucket, stamp `now`
///      iff no timestamp exists. A bucket surplus last tick AND
///      this tick keeps its old timestamp (window accumulates).
///   3. **Elect**: return `(bucket, surplus_count)` pairs where the
///      window has elapsed. Caller deletes up to `surplus_count`
///      Jobs from each.
///
/// The `&mut BTreeMap` is the per-pool entry in `Ctx::manifest_idle`
/// (lock held for the duration of this call — a few map ops).
///
/// `now` as a parameter (not `Instant::now()` inline) → tests can
/// simulate elapsed time by passing `now + window + 1s` without
/// real sleeps. Same pattern as `check_stabilization` in
/// `scaling/mod.rs`.
pub(super) fn update_idle_and_reapable(
    idle_since: &mut BTreeMap<Bucket, Instant>,
    surplus: &BTreeMap<Bucket, usize>,
    now: Instant,
    window: Duration,
) -> BTreeMap<Bucket, usize> {
    // Prune: bucket no longer surplus → clock resets. `retain`
    // mutates in place. A bucket that went surplus → demand
    // returned at 300s → surplus again at 400s starts its 600s
    // window FROM 400s, not from 0s. Plan T4 case 3.
    idle_since.retain(|b, _| surplus.contains_key(b));

    // Record: new surplus buckets get stamped. `or_insert` (not
    // `insert`) — an already-stamped bucket keeps its timestamp.
    for &b in surplus.keys() {
        idle_since.entry(b).or_insert(now);
    }

    // Elect: window elapsed → reapable. `>=` not `>`: a bucket idle
    // for EXACTLY the window is reapable (matches the plan's 601s
    // test, avoids a dead second between 600 and 601).
    surplus
        .iter()
        .filter(|(b, _)| {
            // idle_since[b] is guaranteed present (just inserted
            // above). `[b]` panics on absence — correct; if it's
            // missing, or_insert is broken.
            now.duration_since(idle_since[b]) >= window
        })
        .map(|(&b, &c)| (b, c))
        .collect()
}

// r[impl ctrl.pool.manifest-scaledown]
/// Select Jobs safe to delete from the reapable buckets.
///
/// "Safe" = pod confirmed idle via `ListExecutors`
/// (`running_builds == 0`). The `executor_id → Job` match relies on
/// K8s Job-pod naming: the pod is `{job_name}-{5-char-random}`, and
/// `executor_id` IS the pod name (`RIO_WORKER_ID=$(POD_NAME)` via
/// downward API in `build_pod_spec` — see `builderpool/mod.rs`
/// cleanup-phase comment).
///
/// Three states per Job:
///   - **Idle** (matching executor, `running_builds == 0`): delete
///   - **Busy** (matching executor, `running_builds > 0`): skip.
///     "Don't delete a Job mid-build." Deleting orphans the build
///     (pod SIGTERM'd mid-compilation → scheduler must reassign).
///   - **Unknown** (no matching executor): skip. Pod starting up
///     (not heartbeating yet), or disconnected between RPC calls.
///     Can't prove idle → conservative. Pod-startup is the common
///     case; a Job we spawned last tick shouldn't be immediately
///     reaped just because the executor hasn't registered.
///
/// Per-bucket cap: at most `reapable[bucket]` Jobs from each bucket.
/// If surplus=3 but only 2 are idle, delete 2. Next tick re-diffs;
/// the remaining 1 (once its build finishes) becomes eligible.
///
/// BTreeMap iteration order → deterministic (stable Job delete
/// ordering across ticks; operators see consistent `kubectl get
/// events` ordering).
///
/// Race window: executor reports idle at RPC-time, scheduler
/// dispatches to it a millisecond later, we delete the Job. Plan
/// risk §2 accepts this — scheduler retries dispatch on the next
/// worker. The window is bounded by one reconcile tick (~10s), and
/// the scheduler's dispatch loop handles transient worker loss
/// anyway (heartbeat timeout → reassign).
// r[impl ctrl.pool.manifest-failed-sweep+2]
/// Select Failed Jobs for sweep. Failed = `status.failed > 0`
/// (`backoff_limit=0` means one pod crash → terminal). No
/// idle-check: a Failed Job's pod has already terminated — nothing
/// to interrupt. Bounded to `cap`; a day-long crash-loop leaves
/// ~8640 Failed Jobs, firing 8640 deletes in one tick would be its
/// own incident. Callers compute `cap` as
/// `max(FAILED_SWEEP_MIN, spec.replicas.max)` so the sweep
/// converges under full crash-loop (accumulation ≤ `replicas.max`
/// per tick).
///
/// The filter is the inverse of the active-Job predicate at the
/// inventory site (`status.failed == 0 && status.succeeded == 0`).
/// A Job with `succeeded > 0` is NOT swept — manifest pods loop
/// (no `RIO_EPHEMERAL=1`), so Complete only happens on deliberate
/// scale-down; the reapable pass already handles those.
pub(super) fn select_failed_jobs(jobs: &[Job], cap: usize) -> Vec<&Job> {
    let mut failed: Vec<&Job> = jobs.iter().filter(|j| is_failed_job(j)).collect();
    // Oldest-first: under backlog, sweep the crashes that have been
    // sitting longest. list() order is apiserver-arbitrary; without
    // this a persistently-oldest Failed Job may never get selected.
    // Option<Time> sorts None-first — a Job with no timestamp
    // (pathological, test mocks) is treated as oldest, which is the
    // safe direction.
    failed.sort_by_key(|j| j.metadata.creation_timestamp.clone());
    failed.truncate(cap);
    failed
}

/// Coarse Failed-Job-count tier for the `CrashLoopDetected` event
/// message. K8s deduplicates events by `(reason, message)` — an
/// exact count changes every tick (sweep races re-create, so the
/// count fluctuates), preventing dedup. Three tiers give operators
/// order-of-magnitude visibility while keeping the message stable
/// enough for the apiserver to collapse per-tick emits.
///
/// Must only be called when `count >= CRASH_LOOP_WARN_THRESHOLD`
/// (the event gate); the base tier is that threshold.
pub(super) fn crash_loop_tier(count: usize) -> &'static str {
    debug_assert!(count >= CRASH_LOOP_WARN_THRESHOLD);
    match count {
        50.. => "50+",
        10.. => "10+",
        _ => "3+",
    }
}

pub(super) fn select_deletable_jobs<'a>(
    active_jobs: &[&'a Job],
    reapable: &BTreeMap<Bucket, usize>,
    executors: &[ExecutorInfo],
) -> Vec<&'a Job> {
    // Per-bucket delete budget. Decremented as we select; exhausted
    // → skip remaining Jobs in that bucket.
    let mut budget: BTreeMap<Bucket, usize> = reapable.clone();

    let mut out = Vec::new();
    for &job in active_jobs {
        // Bucket: floor (None) or unparseable → skip (not tracked).
        let Some(bucket) = parse_bucket_from_labels(job) else {
            continue;
        };
        // Reapable + budget left?
        let Some(remaining) = budget.get_mut(&bucket) else {
            continue;
        };
        if *remaining == 0 {
            continue;
        }
        // Idle check. Job name → pod-name prefix. `{job}-` with
        // trailing dash: Job "pool-mf-8g-2000m-abc" matches pod
        // "pool-mf-8g-2000m-abc-xyzwq", NOT pod of a different Job
        // "pool-mf-8g-2000m-abcdef-xyzwq".
        let Some(job_name) = job.metadata.name.as_deref() else {
            continue; // Job without a name — can't delete by name anyway
        };
        let prefix = format!("{job_name}-");
        match executors
            .iter()
            .find(|e| e.executor_id.starts_with(&prefix))
        {
            Some(e) if e.running_builds == 0 => {
                // Confirmed idle. Safe to delete.
                out.push(job);
                *remaining -= 1;
            }
            Some(_) => {
                // Busy. Skip. Plan T4 case 4: 3 surplus, 1 busy →
                // delete at most 2.
            }
            None => {
                // Unknown. Conservative skip (pod-startup or RPC
                // race). If this Job's pod never registers, it'll
                // eventually crash/get-stuck and Job status goes
                // Failed → filtered from active_jobs → replaced.
            }
        }
    }
    out
}

/// Build `ResourceRequirements` from a bucket. Both requests AND
/// limits set to the bucket values — manifest mode is precise-fit,
/// not burst-friendly. A 48Gi bucket means the scheduler measured
/// 48Gi; requesting less risks OOM, limiting more wastes node
/// capacity (kube-scheduler packs by limits).
fn bucket_to_resources(bucket: Bucket) -> ResourceRequirements {
    let (mem_bytes, cpu_m) = bucket;
    // Quantity string format: raw bytes for memory (K8s accepts
    // "8589934592" same as "8Gi"; using bytes avoids a second
    // conversion), millicores for cpu.
    let mem = Quantity(mem_bytes.to_string());
    let cpu = Quantity(format!("{cpu_m}m"));
    let mut m = BTreeMap::new();
    m.insert("memory".to_string(), mem);
    m.insert("cpu".to_string(), cpu);
    ResourceRequirements {
        requests: Some(m.clone()),
        limits: Some(m),
        ..Default::default()
    }
}

// r[impl ctrl.pool.manifest-reconcile]
// r[impl ctrl.pool.manifest-labels]
/// Build a K8s Job for one manifest-mode pod.
///
/// REUSES `build_pod_spec` (same as ephemeral's `build_job`) with
/// `resources_override: Some(bucket_to_resources(bucket))` when
/// `bucket` is Some; `None` when cold-start (→ `spec.resources`
/// floor, same as the STS path).
///
/// Differences from `ephemeral::build_job`:
///   - NO `RIO_EPHEMERAL=1` (pod loops for multiple builds)
///   - NO `activeDeadlineSeconds` (long-lived; wrong-pool-spawn
///     isn't a concern — manifest pods are demand-driven by the
///     bucket diff, not by cluster-wide queue depth)
///   - `RIO_MAX_BUILDS=1` force-override (CEL enforces
///     `maxConcurrentBuilds==1` for Manifest; defensive override
///     for pre-CEL specs, same as ephemeral)
///   - Labels: `rio.build/sizing=manifest` + class labels
///   - Name: `{pool}-mf-{mem}g-{cpu}m-{random6}`
pub(super) fn build_manifest_job(
    wp: &BuilderPool,
    oref: OwnerReference,
    scheduler: &SchedulerAddrs,
    store_addr: &str,
    bucket: Option<Bucket>,
) -> Result<Job> {
    let pool = wp.name_any();

    let cache_quantity = Quantity(wp.spec.fuse_cache_size.clone());
    let cache_gb = builders::parse_quantity_to_gb(&wp.spec.fuse_cache_size)?;

    // resources_override: Some → bucket; None → spec.resources floor.
    // T1's signature change is what makes this possible.
    let resources_override = bucket.map(bucket_to_resources);
    let mut pod_spec = builders::build_pod_spec(
        wp,
        scheduler,
        store_addr,
        cache_gb,
        cache_quantity,
        resources_override,
    );

    // r[impl ctrl.pool.manifest-single-build]
    // Force RIO_MAX_BUILDS=1. CEL enforces `sizing=Manifest →
    // maxConcurrentBuilds==1` at admission; this is defensive for
    // pre-CEL specs. Same find-and-replace as ephemeral (not push-
    // -duplicate — K8s env is last-wins but depending on that is
    // fragile).
    for e in pod_spec.containers[0]
        .env
        .as_mut()
        .ok_or_else(|| {
            Error::InvalidSpec("build_container produced a container with no env".into())
        })?
        .iter_mut()
    {
        if e.name == "RIO_MAX_BUILDS" {
            e.value = Some("1".into());
        }
    }

    // restartPolicy: Never. Required by K8s for Jobs with
    // backoffLimit=0. Same reasoning as ephemeral: if the pod
    // crashes (OOM on a derivation that exceeded its estimate),
    // the SCHEDULER owns retry (reassign to a larger-bucket pod
    // next cycle, or update the estimate). K8s restarting the
    // same pod on the same node is a tight-loop risk.
    pod_spec.restart_policy = Some("Never".into());

    // Labels: pool labels + sizing mode + class. The class labels
    // are what inventory_by_bucket reads — THIS is the round-trip
    // boundary. bucket_labels() is the single formatting point.
    let mut labels = builders::labels(wp);
    labels.insert(SIZING_LABEL.into(), SIZING_MANIFEST.into());
    let (mem_class, cpu_class) = bucket_labels(bucket);
    labels.insert(MEMORY_CLASS_LABEL.into(), mem_class.clone());
    labels.insert(CPU_CLASS_LABEL.into(), cpu_class.clone());

    // Name: `{pool}-mf-{mem}g-{cpu}m-{random6}`. The class in the
    // name is operator-ergonomic (`kubectl get jobs` shows the
    // size at a glance) not functional (labels carry the data).
    // DNS-1123: lowercase alnum + '-'. `Gi` → `g`, `m` stays.
    // `floor` stays as-is (5 chars, valid).
    let suffix = random_suffix();
    let mem_tag = mem_class.strip_suffix("Gi").unwrap_or(&mem_class);
    let cpu_tag = cpu_class.strip_suffix('m').unwrap_or(&cpu_class);
    // 63-char limit: `{pool}-mf-{n}g-{n}m-{6}`. With realistic
    // numbers (pool<20, mem≤4 digits, cpu≤5 digits), ~40 chars.
    // Overlong pool name → K8s rejects with a clear error.
    let job_name = format!("{pool}-mf-{mem_tag}g-{cpu_tag}m-{suffix}");

    Ok(Job {
        metadata: ObjectMeta {
            name: Some(job_name),
            namespace: wp.namespace(),
            owner_references: Some(vec![oref]),
            labels: Some(labels.clone()),
            ..Default::default()
        },
        spec: Some(JobSpec {
            parallelism: Some(1),
            completions: Some(1),
            backoff_limit: Some(0),
            // r[impl ctrl.pool.manifest-long-lived]
            // NO ttl_seconds_after_finished. Manifest pods loop
            // (no RIO_EPHEMERAL=1), so "finished" only happens on
            // crash/OOM. A crashed Job is already filtered from
            // active_jobs (status.failed > 0 check in the inventory
            // pass), so it doesn't count as supply → replacement
            // spawns next tick. TTL-based reaping would race the
            // controller's deliberate scale-down deletes. Controller
            // deletion (scale-down above) + ownerRef GC (BuilderPool
            // delete) are the ONLY Job-removal paths.
            //
            // Crashed Jobs are swept by select_failed_jobs in the
            // scale-down pass. Spec: `ctrl.pool.manifest-failed-
            // sweep` in docs/src/components/controller.md. No TTL
            // here: TTL-based reaping would race deliberate
            // scale-down deletes.
            //
            // NO active_deadline_seconds — manifest pods are
            // long-lived. Scale-down (above) handles the
            // demand-dropped case.
            template: PodTemplateSpec {
                metadata: Some(ObjectMeta {
                    labels: Some(labels),
                    ..Default::default()
                }),
                spec: Some(pod_spec),
            },
            ..Default::default()
        }),
        ..Default::default()
    })
}
