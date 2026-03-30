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
//!   4. **Diff**: per bucket, `deficit = demand.saturating_sub(supply)`.
//!      Over-provisioned (supply > demand) → zero spawns. Scale-DOWN
//!      (reaping surplus Jobs) is P0505's job — this reconciler only
//!      scales UP.
//!   5. **Cold-start floor**: `queued_derivations - manifest.len()`
//!      derivations have no `build_history` sample → manifest omits
//!      them (proto doc at `admin_types.proto:249`). Spawn those at
//!      `spec.resources` (the operator-configured floor).
//!   6. **Spawn**: per deficit, `build_pod_spec(..., Some(resources))`.
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
use std::time::Duration;

use k8s_openapi::api::batch::v1::{Job, JobSpec};
use k8s_openapi::api::core::v1::{PodTemplateSpec, ResourceRequirements};
use k8s_openapi::apimachinery::pkg::api::resource::Quantity;
use k8s_openapi::apimachinery::pkg::apis::meta::v1::OwnerReference;
use kube::api::{Api, ListParams, ObjectMeta, PostParams};
use kube::runtime::controller::Action;
use kube::{CustomResourceExt, Resource, ResourceExt};
use rio_proto::types::{DerivationResourceEstimate, GetCapacityManifestRequest};
use tracing::{debug, info, warn};

use crate::crds::builderpool::BuilderPool;
use crate::error::{Error, Result};
use crate::reconcilers::Ctx;

use super::builders::{self, SchedulerAddrs};
use super::{MANAGER, POOL_LABEL};

/// Requeue interval. Same as ephemeral (~10s) — manifest is demand-
/// driven, not drift-driven. A derivation queued between ticks waits
/// one interval before a pod is spawned for it. See ephemeral.rs's
/// `EPHEMERAL_REQUEUE` doc for the latency-vs-apiserver-load tradeoff.
const MANIFEST_REQUEUE: Duration = Duration::from_secs(10);

/// `ttlSecondsAfterFinished` on manifest Jobs. Manifest pods loop
/// (no `RIO_EPHEMERAL=1`), so "finished" = worker crashed/OOM'd or
/// scheduler drained it. 60s matches ephemeral — long enough for
/// `kubectl logs` debugging, short enough that dead Jobs don't pile
/// up in the inventory count (a Failed-but-unreap'd Job with the
/// right labels would count as supply → under-spawn).
const JOB_TTL_SECS: i32 = 60;

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

/// `(est_memory_bytes, est_cpu_millicores)` — the grouping key.
/// Scheduler-side bucketing (admin_types.proto:234) rounds memory to
/// nearest 4GiB, cpu to nearest 2000m, so the keyspace is coarse by
/// design. A BTreeMap over this is small (typical queue has <10
/// distinct buckets).
pub(super) type Bucket = (u64, u32);

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
    let ns = wp
        .namespace()
        .ok_or_else(|| Error::InvalidSpec("BuilderPool has no namespace".into()))?;
    let name = wp.name_any();
    let jobs_api: Api<Job> = Api::namespaced(ctx.client.clone(), &ns);

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
    let active_jobs: Vec<&Job> = jobs
        .items
        .iter()
        .filter(|j| {
            let s = j.status.as_ref();
            s.and_then(|st| st.succeeded).unwrap_or(0) == 0
                && s.and_then(|st| st.failed).unwrap_or(0) == 0
        })
        .collect();
    let supply = inventory_by_bucket(&active_jobs);
    let cold_start_supply = active_jobs.iter().filter(|j| is_floor_job(j)).count();
    let active_total: i32 = active_jobs.len().try_into().unwrap_or(i32::MAX);

    // ---- Diff ----
    let plan = compute_spawn_plan(&demand, &supply, cold_start, cold_start_supply);
    let to_spawn: usize = plan.iter().map(|d| d.count).sum();

    // ---- Spawn ----
    // Ceiling: same `spec.replicas.max` cap as ephemeral. A manifest
    // with 200 distinct derivations shouldn't spawn 200 pods if the
    // operator said max=10. We TRUNCATE the plan at the ceiling —
    // BTreeMap iteration order means smaller buckets spawn first
    // (predictable; small pods are cheaper to waste on the wrong
    // bucket). Next tick re-diffs with the updated supply.
    let ceiling = wp.spec.replicas.max;
    let headroom = ceiling.saturating_sub(active_total).max(0) as usize;
    let budget = to_spawn.min(headroom);
    let truncated = truncate_plan(&plan, budget);

    if !truncated.is_empty() {
        let oref = wp.controller_owner_ref(&()).ok_or_else(|| {
            Error::InvalidSpec("BuilderPool has no metadata.uid (not from apiserver?)".into())
        })?;
        let scheduler = SchedulerAddrs {
            addr: ctx.scheduler_addr.clone(),
            balance_host: ctx.scheduler_balance_host.clone(),
            balance_port: ctx.scheduler_balance_port,
        };

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
                match jobs_api.create(&PostParams::default(), &job).await {
                    Ok(_) => {
                        info!(
                            pool = %name, job = %job_name,
                            bucket = ?directive.bucket,
                            "spawned manifest Job"
                        );
                    }
                    Err(kube::Error::Api(ae)) if ae.code == 409 => {
                        debug!(pool = %name, job = %job_name, "Job name collision; will retry");
                    }
                    Err(e) => return Err(e.into()),
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

    // ---- Status patch ----
    // Same shape as ephemeral: `replicas` = active Jobs, `desired` =
    // ceiling. SchedulerUnreachable condition reflects BOTH RPCs
    // (either down → True). Reusing the ephemeral helper — same
    // semantics, different RPC name in the message doesn't matter
    // for the condition type.
    let wp_api: Api<BuilderPool> = Api::namespaced(ctx.client.clone(), &ns);
    let ar = BuilderPool::api_resource();
    let prev = crate::scaling::find_condition(wp, "SchedulerUnreachable");
    let cond =
        super::ephemeral::scheduler_unreachable_condition(scheduler_err.as_deref(), prev.as_ref());
    let status_patch = serde_json::json!({
        "apiVersion": ar.api_version,
        "kind": ar.kind,
        "status": {
            "replicas": active_total,
            "readyReplicas": active_total,
            "desiredReplicas": ceiling,
            "conditions": [cond],
        },
    });
    wp_api
        .patch_status(
            &name,
            &kube::api::PatchParams::apply(MANAGER).force(),
            &kube::api::Patch::Apply(&status_patch),
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
/// bucket (no negative spawn; scale-down is P0505's job).
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
    // first — if the ceiling truncates the plan mid-spawn, we've
    // spawned the cheap pods. Next tick retries the expensive ones.
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
    let suffix = super::ephemeral::random_suffix();
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
            ttl_seconds_after_finished: Some(JOB_TTL_SECS),
            // NO active_deadline_seconds — manifest pods are
            // long-lived. P0505 (scale-down grace) will drain them
            // via DrainExecutor when the bucket's demand drops.
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
