//! Pool Job-per-build reconciler.
//!
//!   1. Each `apply()` tick polls `GetSpawnIntents` (filtered by
//!      `{kind, systems, features}`) via the same `ctx.admin` client
//!      the finalizer uses for DrainExecutor.
//!   2. If the scheduler returned intents and active Jobs for this
//!      pool < `spec.maxConcurrent`, spawn one Job per intent (up to
//!      the ceiling).
//!   3. Each Job runs one rio-builder pod → worker exits after one
//!      build → pod terminates → `ttlSecondsAfterFinished`
//!      ([`JOB_TTL_SECS`]) reaps the Job.
//!
//! From the scheduler's perspective a Job pod is just an executor:
//! it heartbeats in, gets a dispatch, sends CompletionReport,
//! disconnects. The "ephemeral" property is purely worker-side
//! (exit after one build) + controller-side (Job lifecycle).
//!
//! # Job naming
//!
//! `rio-{kind}-{pool}-{intent-suffix}` — suffix derives from
//! `intent_id` (= drv_hash, nixbase32) so a re-polled still-Ready
//! intent re-creates the SAME Job name and the apiserver's
//! NameCollision dedupes (cold-start re-spawn would otherwise fire
//! one pod per reconcile tick).
//!
//! # Zero cross-build state
//!
//! Fresh pod = fresh emptyDir for FUSE cache + overlays. An
//! untrusted tenant CANNOT leave poisoned cache entries for the
//! next build — there is no "next build" on that pod.

use std::collections::{BTreeMap, HashMap, HashSet};

use k8s_openapi::api::batch::v1::Job;
use k8s_openapi::api::core::v1::{
    NodeAffinity, NodeSelector, Pod, PodSpec, ResourceRequirements, Toleration,
};
use k8s_openapi::apimachinery::pkg::api::resource::Quantity;
use k8s_openapi::apimachinery::pkg::apis::meta::v1::OwnerReference;
use kube::api::{Api, DeleteParams, ListParams};
use kube::runtime::controller::Action;
use kube::{Resource, ResourceExt};
use tracing::{debug, info, warn};

#[cfg(test)]
use super::job::JOB_TTL_SECS;
use super::job::{
    JOB_REQUEUE, REAP_PENDING_GRACE, ephemeral_job, is_active_job, is_pending_job, job_census,
    job_older_than, patch_job_pool_status, reap_excess_pending, reap_orphan_running,
    report_deadline_exceeded_jobs, report_terminated_pods, spawn_for_each,
};
use super::pod::{self, UpstreamAddrs};
use crate::error::{Error, Result};
use crate::reconcilers::admin_call;
use crate::reconcilers::node_informer::HwClassConfig;
use crate::reconcilers::{Ctx, KubeErrorExt, require_namespace};
use rio_crds::pool::{ExecutorKind, Pool};
use rio_proto::types::SpawnIntent;

/// Pod-template annotation carrying `SpawnIntent.intent_id`. Read by
/// the builder via downward-API → `RIO_INTENT_ID` → heartbeat.
pub(crate) const INTENT_ID_ANNOTATION: &str = "rio.build/intent-id";

/// Job-metadata annotation carrying a fingerprint of
/// `SpawnIntent.node_selector`. Compared on each tick so a Pending Job
/// whose selector no longer matches the scheduler's current solve
/// (ICE-backoff spot→on-demand fallback) is reaped instead of
/// NameCollision-blocking the re-solved intent forever.
pub(crate) const INTENT_SELECTOR_ANNOTATION: &str = "rio.build/intent-selector";

/// Pod-template annotation carrying the controller's create-time
/// bench-gate decision. `"true"` ⇒ the spawned builder runs the full
/// K=3 microbench (STREAM/ioseq/alu) before accepting work. Read via
/// downward-API → `RIO_HW_BENCH_NEEDED` (`rio_builder::Config.
/// hw_bench_needed`). ADR-023 §13a: fail-closed on the BUILDER side
/// (annotation absent → skip K=3, only the scalar `alu` probe runs).
pub(crate) const HW_BENCH_NEEDED_ANNOTATION: &str = "rio.build/hw-bench-needed";

/// Log + scratch budget. nix `build-dir` lands in the overlay emptyDir
/// (nix ≥2.30 default = stateDir/builds), but stdout/stderr capture and
/// the daemon's own state live outside. 1 GiB headroom.
const LOG_BUDGET_BYTES: u64 = 1 << 30;
/// Overlay emptyDir sizeLimit headroom multiplier on `disk_bytes` when
/// `SpawnIntent.disk_headroom_factor` is absent/zero (pre-ADR-023
/// scheduler skew). The variance-aware `headroom(n_eff)` curve is
/// computed scheduler-side and carried on the intent; this is the flat
/// fallback only.
pub(crate) const OVERLAY_HEADROOM_FALLBACK: f64 = 1.5;

/// Resolve the overlay-disk headroom multiplier for an intent.
/// `disk_headroom_factor` is `optional` on the wire so a pre-§sizing
/// scheduler decodes as `None`; 0.0 (proto default for `double`) is
/// also treated as absent.
pub(crate) fn intent_headroom(i: &SpawnIntent) -> f64 {
    i.disk_headroom_factor
        .filter(|&h| h > 0.0)
        .unwrap_or(OVERLAY_HEADROOM_FALLBACK)
}

/// Pod `ephemeral-storage` request/limit for an intent's `disk_bytes`
/// plus a per-pool FUSE-cache budget.
///
/// = `disk_bytes × headroom` (overlay emptyDir, prjquota fit × the
/// scheduler's variance-aware `headroom(n_eff)` cushion) +
/// `fuse_cache_bytes` (input closure, the `fuse-cache` emptyDir
/// sizeLimit) + [`LOG_BUDGET_BYTES`] (stdout/stderr capture + daemon
/// state outside the overlay).
///
/// Single source for FOUR callers that must agree (all via
/// [`intent_pod_footprint`] or this fn directly):
/// - [`apply_intent_resources`] — the actual pod request/limit;
/// - [`crate::reconcilers::nodeclaim_pool::ffd::simulate`] — the FFD
///   sim's fit-check decrement (raw `disk_bytes` here while the pod
///   requests 1.5× + 50Gi + 1Gi meant FFD over-places ~50× on the
///   disk axis);
/// - [`crate::reconcilers::nodeclaim_pool`]'s `cover_deficit` — the
///   NodeClaim `resources.requests.ephemeral-storage` floor (B8 live:
///   a 100Gi-intent pod asked 201Gi on a 189Gi-allocatable node);
/// - helm-lint `14-disk-ceiling.sh` — `karpenter.dataVolumeSize` ≥
///   `pod_ephemeral_request(sla.maxDisk, worst-case headroom,
///   poolDefaults.fuseCacheBytes)` + kubelet reserve.
// r[impl sched.sla.disk-reaches-ephemeral-storage]
pub(crate) fn pod_ephemeral_request(disk_bytes: u64, headroom: f64, fuse_cache_bytes: u64) -> u64 {
    ((disk_bytes as f64 * headroom) as u64)
        .saturating_add(fuse_cache_bytes)
        .saturating_add(LOG_BUDGET_BYTES)
}

/// `(cores, mem, ephemeral-storage)` triple a pod for `i` will
/// actually request — the SHARED accounting [`apply_intent_resources`]
/// stamps and [`crate::reconcilers::nodeclaim_pool::ffd::simulate`]
/// fit-checks. FFD's contract is "predicts what kube-scheduler will
/// do"; the only way that holds is for both sides to compute the same
/// triple from the same fn (§Simulator-shares-accounting).
pub(crate) fn intent_pod_footprint(i: &SpawnIntent, fuse_cache_bytes: u64) -> (u32, u64, u64) {
    (
        i.cores,
        i.mem_bytes,
        pod_ephemeral_request(i.disk_bytes, intent_headroom(i), fuse_cache_bytes),
    )
}

/// Margin between the worker's `daemon_timeout` and K8s
/// `activeDeadlineSeconds`, so the worker's `tokio::time::timeout`
/// fires first and emits `CompletionReport{TimedOut}` (telemetry +
/// `handle_timeout_failure` cap-check) before K8s SIGKILLs.
const WORKER_DEADLINE_SLACK_SECS: i64 = 90;

/// `schedulerName` set on builder pods. The helm-deployed second
/// kube-scheduler (B2, MostAllocated scoring) is the only scheduler
/// watching this name; the default kube-scheduler ignores
/// kube-build-scheduler pods entirely.
pub(crate) const KUBE_BUILD_SCHEDULER: &str = "kube-build-scheduler";

/// `priorityClassName` prefix; suffix is [`priority_bucket`]. B2 renders
/// 10 fixed PriorityClasses `rio-builder-prio-{0..9}` with
/// `preemptionPolicy: Never` so a high-bucket pod sorts ahead in
/// kube-build-scheduler's queue without evicting a running low-bucket build.
pub(crate) const PRIORITY_CLASS_PREFIX: &str = "rio-builder-prio-";

/// ADR-023 §13b priority bucket: `⌊log₂ c*⌋` clamped to `[0, 9]`. The
/// scheduler's `SlaConfig::validate` asserts `maxCores < 1024 = 2¹⁰` so
/// the clamp is reachable only via that config's ceiling, not normal
/// solves. `cores = 0` (proto default; FOD/unfitted intents may emit
/// it) → bucket 0. Larger builds sort first in kube-build-scheduler's active
/// queue so the FFD sim's largest-first packing is honoured at bind
/// time — the keystone that makes [`super::super::nodeclaim_pool::
/// PlaceableGate`]'s prediction self-fulfilling.
// r[impl ctrl.nodeclaim.priority-bucket]
pub(super) fn priority_bucket(cores: u32) -> u32 {
    cores.checked_ilog2().unwrap_or(0).min(9)
}

/// `activeDeadlineSeconds` for an ephemeral Job: `intent.
/// deadline_secs` verbatim. The scheduler computes it per-derivation
/// (D7: `wall_p99 × 5` for fitted, `[sla].probe.deadline_secs` for
/// unfitted, clamped `[floor, 86400]`) and `SlaConfig::validate`
/// guarantees `probe.deadline_secs >= 180`, so the intent value is
/// always `>= 180`. No controller-side multiplier or per-kind
/// fallback. `.max(180)` is defensive only — proto default is 0; a
/// 0s deadline would fail the Job at creation, and `< 180` would tie
/// the worker's `daemon_timeout = deadline − 90` against this timer.
// r[impl ctrl.ephemeral.intent-deadline]
pub(super) fn ephemeral_deadline(intent: &SpawnIntent) -> i64 {
    i64::from(intent.deadline_secs).max(180)
}

/// `hw_class` strings the `HwClassSampled` RPC keys on for one
/// intent's allowed-set `A`. The scheduler emits `hw_class_names[i]`
/// alongside `node_affinity[i]` (one `$h` per `(h, cap)` cell), so
/// this is a straight read — no label-reconstruction (bug_061: the
/// previous `HwClass::from_selector_term` reverse-engineered `$h` from
/// a hardcoded 4-label tuple, which was wrong for any operator whose
/// `[sla.hw_classes.$h].labels` schema differed). The same `h` may
/// appear under both spot and on-demand — [`HwSampledCache::fetch`]
/// dedupes across the whole tick.
pub(super) fn hw_classes_in(intent: &SpawnIntent) -> impl Iterator<Item = String> + '_ {
    intent.hw_class_names.iter().cloned()
}

/// Per-tick `HwClassSampled` snapshot: `h → [distinct-tenant; K]`
/// plus the scheduler's `trust_threshold` (= `FLEET_MEDIAN_MIN_TENANTS`)
/// from the scheduler's `HwTable` (~60s stale at worst). One RPC per
/// pool-reconcile tick covers every intent — the request is the union
/// of `hw_classes_in` over all intents this tick.
///
/// RPC failure / scheduler unreachable → empty map. Unknown `h` reads
/// as `[0; K]` in [`Self::any_under_threshold`], so an outage marks
/// `hw-bench-needed=true` on every affinity-carrying intent that
/// clears the mem floor — over-benching, never under-benching. The
/// mem-floor gate keeps STREAM's ~4.6 GiB working set off small pods
/// regardless.
#[derive(Default)]
pub(crate) struct HwSampledCache {
    sampled: HashMap<String, rio_proto::types::HwDimCounts>,
    /// `cross_tenant_median`'s per-dim `min_tenants` gate floor — the
    /// scheduler's `FLEET_MEDIAN_MIN_TENANTS`, carried in the response
    /// so unit, granularity, AND value share one source of truth.
    /// merged_bug_001: a controller-side hardcoded `3` left a 3..5
    /// dead band where the controller stopped K=3-benching but the
    /// scheduler still pinned `factor=[1.0;K]` — calibration deadlock.
    trust_threshold: u32,
}

impl HwSampledCache {
    /// One `HwClassSampled` RPC for the given (deduped) classes.
    /// Empty input → empty cache (no RPC) so non-hw-targeted ticks
    /// (FOD-only, fetcher pools) cost nothing.
    pub(crate) async fn fetch(ctx: &Ctx, hw_classes: HashSet<String>) -> Self {
        if hw_classes.is_empty() {
            return Self::default();
        }
        match admin_call(ctx.admin.clone().hw_class_sampled(
            rio_proto::types::HwClassSampledRequest {
                hw_classes: hw_classes.into_iter().collect(),
            },
        ))
        .await
        {
            Ok(r) => {
                let r = r.into_inner();
                Self {
                    sampled: r.sampled_count,
                    // Field absent ⇒ old-scheduler skew. Fall back to
                    // the new-scheduler value (5): over-benches the
                    // 3..5 band rather than reintroducing the deadlock.
                    // `HwTable::factor` ignores under-threshold dims so
                    // duplicate benches before then are harmless.
                    trust_threshold: r.trust_threshold.unwrap_or(5),
                }
            }
            Err(e) => {
                warn!(error = %e, "HwClassSampled poll failed; treating all as undersampled");
                Self::default()
            }
        }
    }

    /// `∃ h ∈ A, ∃ d ∈ K : tenants_with_dim(h, d) < trust_threshold`.
    /// `A = ∅` (no `node_affinity`) is vacuously false — the actual
    /// `h` is unknown until kube-scheduler bind, so the create-time
    /// check cannot be applied; the builder still runs the scalar
    /// `alu` probe. Unknown `h` (or empty `per_dim` — proto default)
    /// reads as under-threshold. bug_013: the per-dim quantifier
    /// mirrors `cross_tenant_median`'s gate so honest pods K=3-bench
    /// until EVERY dim has ≥`trust_threshold` tenants, denying
    /// single-tenant capture.
    pub(crate) fn any_under_threshold<I>(&self, a: I) -> bool
    where
        I: IntoIterator<Item = String>,
    {
        a.into_iter().any(|h| {
            self.sampled
                .get(&h)
                .filter(|c| !c.per_dim.is_empty())
                .is_none_or(|c| c.per_dim.iter().any(|&n| n < self.trust_threshold))
        })
    }

    /// Test-only constructor: per-hw_class K=3 distinct-tenant counts
    /// + the threshold to compare against.
    #[cfg(test)]
    pub(crate) fn from_parts(m: HashMap<String, [u32; 3]>, trust_threshold: u32) -> Self {
        Self {
            sampled: m
                .into_iter()
                .map(|(h, n)| (h, rio_proto::types::HwDimCounts { per_dim: n.into() }))
                .collect(),
            trust_threshold,
        }
    }
}

// r[impl ctrl.pool.ephemeral]
/// Reconcile a Pool: count active Jobs, poll spawn intents, spawn
/// Jobs if work is waiting.
///
/// Status: `replicas` / `readyReplicas` / `desiredReplicas` mean
/// "active Jobs." `desiredReplicas` is the concurrent-Job ceiling
/// (`spec.maxConcurrent`).
pub(super) async fn reconcile(pool: &Pool, ctx: &Ctx) -> Result<Action> {
    // Namespace-missing is `InvalidSpec` not `NotFound` — a Pool CR
    // without `.metadata.namespace` is a cluster-scoped apply error
    // (the CRD is `Namespaced`), not a transient condition.
    let ns = require_namespace(pool)?;
    let name = pool.name_any();
    let jobs_api: Api<Job> = Api::namespaced(ctx.client.clone(), &ns);
    let pods_api: Api<Pod> = Api::namespaced(ctx.client.clone(), &ns);
    // The no-`.metadata.uid` error only happens on a CR not read from
    // the apiserver — tests that construct one in memory forget this;
    // production reconcile always has it.
    let oref = pool.controller_owner_ref(&()).ok_or_else(|| {
        Error::InvalidSpec("Pool has no metadata.uid (not from apiserver?)".into())
    })?;
    let ceiling = pool.spec.max_concurrent.map(|c| c as i32);

    // ---- Poll spawn intents ----
    // One GetSpawnIntents RPC per reconcile per pool. If the scheduler
    // is unreachable: log + treat as queued=0 + requeue. Next tick
    // retries. We ALSO set a SchedulerUnreachable condition on the
    // Pool so operators can see WHY nothing is spawning.
    let (mut intents, scheduler_err): (Vec<SpawnIntent>, Option<String>) =
        match queued_for_pool(ctx, pool).await {
            Ok(intents) => (intents, None),
            Err(e) => {
                warn!(
                    pool = %name, error = %e,
                    "spawn-intents poll failed; treating as queued=0, will retry"
                );
                (Vec::new(), Some(e.to_string()))
            }
        };
    // r[impl ctrl.nodeclaim.placeable-gate+2]
    // ADR-023 §13b placeable gate: Builder Jobs spawn only for intents
    // the nodeclaim_pool reconciler's last FFD sim placed on a
    // `Registered=True` NodeClaim — structurally closes the spawn-
    // intent fan-out (1226 Ready intents would otherwise mint 1226
    // Pending Jobs, each a Karpenter provisioning request). The
    // controller now knows what's placeable; the scheduler-side cap
    // that DESIGN.md rejected (it couldn't see node state) is resolved
    // here.
    //
    // Builder-only: Fetcher pods land on the helm `rio-fetcher`
    // NodePool; the FFD sim's `OWNER_LABEL` selector covers builder
    // NodeClaims only, so gating Fetcher spawn on it stalls FODs until
    // a builder node boots. `Ctx.placeable = None` ⇔ NodeClaim CRD
    // absent (k3s VM tests without Karpenter) — the gate is
    // structurally a no-op there.
    //
    // `gate_armed` answers "is `queued` authoritative for
    // `reap_excess_pending`?". When the gate exists (CRD present),
    // queued is the FFD-filtered count (Builder) or the raw scheduler
    // count (Fetcher) — both authoritative, reap active. When the gate
    // is absent (CRD absent), `gate_armed=false` keeps reap fail-closed:
    // pre-§13b semantics, where the unfiltered queued count alone is
    // not safe to reap against (a Job in the post-completion
    // `{succeeded:0,ready:0}` window before Job-controller sync looks
    // pending; reap deletes it racing the job-tracking finalizer —
    // ci-failure-patterns "job-tracking finalizer orphan").
    let gate_armed = match (&ctx.placeable, pool.spec.kind) {
        (Some(g), ExecutorKind::Builder) => g.retain(&mut intents),
        (Some(_), _) => true,
        (None, _) => false,
    };
    let queued = intents.len().min(u32::MAX as usize) as u32;

    // ---- HwClassSampled (per-tick, one RPC for the union of A's) ----
    // r[impl ctrl.pool.hw-bench-needed+2]
    //
    // TODO: §13c §one-step-removed: kvm intents now carry
    // `hw_class_names=[metal-*]` so a cold-start metal class triggers a
    // STREAM bench on a `.metal` host (~$5-10/hr). That cost is not new
    // (the static metal NodePool incurred it too), but the bench result
    // matters less for od-only feature classes — the per-class pricing
    // is almost flat. Add `HwClassDef.skip_bench: bool` (or gate on
    // `!provides_features.is_empty()`) to skip the bench for classes
    // where the cost ladder is cheaper to seed directly.
    let hw_sampled =
        HwSampledCache::fetch(ctx, intents.iter().flat_map(hw_classes_in).collect()).await;

    // ---- Count active Jobs for this pool ----
    // ORDERING (I-183): list AFTER the queued poll. The reap step
    // compares `pending` against `queued`. Polling first keeps the
    // comparison coherent.
    let jobs = jobs_api
        .list(&ListParams::default().labels(&format!("{}={name}", super::POOL_LABEL)))
        .await?;
    let census = job_census(&jobs.items);

    // ---- Reap stale Jobs blocking respawn ----
    // (a) Terminal: a drv that re-enters Ready after its prior Job
    //     went Complete/Failed would NameCollision against the stale
    //     terminal Job for JOB_TTL_SECS (600s). (b) Selector-drift: a
    //     Pending Job whose selector no longer matches the scheduler's
    //     re-solve (ICE-backoff) NameCollision-blocks the new intent
    //     forever. Delete both before the spawn pass.
    //
    // Reap sees the FULL intent set, NOT the headroom-truncated
    // slice: when `ceiling` is set and every active slot is a
    // selector-drifted Pending, headroom=0 → truncated slice is empty
    // → reap's `want.is_empty()` early-return fires → nothing freed →
    // headroom stays 0 forever. Reaping frees slots; it doesn't
    // consume headroom, so the cap doesn't apply.
    let reaped =
        reap_stale_for_intents(&jobs_api, &jobs.items, &intents, &name, pool.spec.kind).await;
    // Reaped active Jobs (selector-drifted / orphan Pending) free
    // slots THIS tick; terminal reaped Jobs weren't counted in
    // `census.active` so don't double-count.
    let freed: i32 = jobs
        .items
        .iter()
        .filter(|j| {
            is_active_job(j)
                && j.metadata.deletion_timestamp.is_none()
                && j.metadata
                    .name
                    .as_deref()
                    .is_some_and(|n| reaped.contains(n))
        })
        .count()
        .try_into()
        .unwrap_or(i32::MAX);
    // ---- Spawn decision ----
    // We do NOT subtract `active`: `queued` counts only Ready intents
    // but `active` counts ALL non-terminal Jobs (incl. Running, whose
    // drvs have left the Ready set). Under per-size-class pools that
    // mismatch was bounded by class cutoff (~30s); under one Pool it's
    // bounded by the slowest build, so `queued.sub(active)` starved
    // new Ready drvs for hours (bug_045). `ceiling = None` → uncapped.
    //
    // `census.headroom` recomputes from `(active − freed)` BEFORE the
    // 0-clamp so an over-committed pool (operator lowered
    // `maxConcurrent` while Jobs live) can't overshoot `ceiling`; the
    // pre-JobCensus `clamp(ceiling − active) + freed` form did.
    let headroom = census.headroom(ceiling, freed);

    // Names already present (minus what we just reaped) are skipped
    // in the spawn pass to avoid a create()→409 per still-Ready
    // intent every tick.
    let existing_names: HashSet<String> = jobs
        .items
        .iter()
        .filter_map(|j| j.metadata.name.clone())
        .filter(|n| !reaped.contains(n))
        .collect();

    // Filter-existing BEFORE truncate: `headroom = ceiling - active`
    // already accounts for still-Pending Jobs, but those Jobs' drvs
    // (still Ready, not yet heartbeated) ALSO appear in `intents`. A
    // positional `intents[..headroom]` slice spent slots on them then
    // skipped them in `spawn_for_each` — new intents past index
    // `headroom` were never considered even with free slots. Intents
    // are scheduler-side priority-sorted, so `take(headroom)` over
    // genuinely-new work drops lowest-priority, not HashMap-order.
    let to_spawn_intents: Vec<SpawnIntent> = intents
        .iter()
        .filter(|i| {
            !existing_names.contains(&pod::job_name(
                &name,
                pool.spec.kind,
                &intent_suffix(&i.intent_id),
            ))
        })
        .take(headroom)
        .cloned()
        .collect();

    // r[impl sec.executor.identity-token+2]
    // Mint per-intent `RIO_EXECUTOR_TOKEN`s on a controller-only
    // surface — `SpawnIntent` is plain data (dashboard/CLI also read
    // it via `GetSpawnIntents`), so the credential lives here, not on
    // the intent. One RPC per reconcile per pool, only for the
    // headroom-truncated set the controller is about to create Jobs
    // for. Empty `to_spawn_intents` skips the round-trip; on RPC
    // failure pods spawn without a token (dev-mode parity — the
    // builder omits the header, scheduler rejects under HMAC mode, pod
    // idle-exits, next tick re-spawns). intent_ids the scheduler no
    // longer recognizes (drv left Ready between the two calls) are
    // omitted from the map → same fail-safe.
    let executor_tokens: HashMap<String, String> = if to_spawn_intents.is_empty() {
        HashMap::new()
    } else {
        match admin_call(
            ctx.admin
                .clone()
                .mint_executor_tokens(rio_proto::types::MintExecutorTokensRequest {
                    intent_ids: to_spawn_intents
                        .iter()
                        .map(|i| i.intent_id.clone())
                        .collect(),
                }),
        )
        .await
        {
            Ok(r) => r.into_inner().tokens,
            Err(e) => {
                warn!(
                    pool = %name, error = %e,
                    "mint_executor_tokens failed; spawning without tokens this tick"
                );
                HashMap::new()
            }
        }
    };

    // One pod per intent with that intent's resources + annotation.
    // Headroom truncates; the remainder is picked up next tick after
    // `active` decreases. Under mandatory `[sla]` (Phase 5) the
    // scheduler ALWAYS populates intents — empty list means empty
    // queue → spawns nothing.
    let spawned = spawn_for_each(
        &jobs_api,
        &to_spawn_intents,
        &existing_names,
        &name,
        |intent| {
            build_job(
                pool,
                oref.clone(),
                &ctx.scheduler,
                &ctx.store,
                &ctx.hw_config,
                intent,
                executor_tokens.get(&intent.intent_id).map(String::as_str),
                &hw_sampled,
                ctx.hw_bench_mem_floor,
                ctx.placeable.is_some(),
            )
        },
    )
    .await;
    // Ack to the scheduler so it records `dispatched_cells` for intents
    // that have a Pending Job — both newly spawned AND already-Pending-
    // before-this-tick. The latter covers scheduler restart:
    // `dispatched_cells` is in-memory, so without re-ack a pre-restart
    // Pending Job (deterministic affinity → no reap → no respawn → no
    // fresh ack) never re-arms the §13a heartbeat-edge ICE clear.
    // Scheduler-side `.insert()` overwrites — harmless: deterministic
    // affinity → identical SmallVec contents → overwrite is
    // idempotent-in-effect.
    //
    // Chain `spawned`, NOT `to_spawn_intents`: an intent whose create
    // hit `SpawnOutcome::Failed` (apiserver 5xx, quota 403, webhook
    // reject) has no Job behind it; acking it would leak a
    // `dispatched_cells` entry until the housekeeping DAG-state sweep.
    let pending_job_names: HashSet<String> = jobs
        .items
        .iter()
        .filter(|j| is_pending_job(j))
        .filter_map(|j| j.metadata.name.clone())
        .filter(|n| !reaped.contains(n))
        .collect();
    let to_ack: Vec<SpawnIntent> = intents
        .iter()
        .filter(|i| {
            pending_job_names.contains(&pod::job_name(
                &name,
                pool.spec.kind,
                &intent_suffix(&i.intent_id),
            ))
        })
        .cloned()
        .chain(spawned)
        .collect();
    if to_ack.is_empty() {
        debug!(pool = %name, queued, active = census.active, ?ceiling, "no Pending intents to ack");
    } else {
        if let Err(e) = admin_call(ctx.admin.clone().ack_spawned_intents(
            rio_proto::types::AckSpawnedIntentsRequest {
                spawned: to_ack,
                // §13b NodeClaim watcher (A18) populates both: cells
                // with `Registered=True` edges → `registered_cells`
                // (ICE clear); `Launched=False` / Registered timeout
                // → `unfulfillable_cells` (ICE mark). **A18 must wire
                // `registered_cells` AND `unfulfillable_cells`
                // together** — mark-without-clear climbs backoff
                // unbounded for |A'|>1 intents (heartbeat-clear is
                // |A'|=1-only post-r19).
                unfulfillable_cells: vec![],
                registered_cells: vec![],
                observed_instance_types: vec![],
                // nodeclaim_pool's report_unfulfillable owns the
                // bound-intents stream (full set every tick from
                // PodRequestedCache); the per-pool ack only arms
                // dispatched_cells.
                bound_intents: vec![],
            },
        ))
        .await
        {
            warn!(pool = %name, error = %e, "ack_spawned_intents failed; dispatched_cells not armed this tick");
        }
    }

    // ---- Reap excess Pending ----
    // I-183: spawn-only is half a control loop. `None` when scheduler
    // unreachable OR placeable-gate unarmed: reap is fail-CLOSED (spawn
    // is fail-open).
    let queued_known = (scheduler_err.is_none() && gate_armed).then_some(queued);
    reap_excess_pending(
        &jobs_api,
        &pods_api,
        &jobs.items,
        &reaped,
        queued_known,
        &name,
    )
    .await;

    // ---- Reap orphan Running ----
    // I-165: a builder stuck in D-state (FUSE wait, OOM-loop) can't
    // self-exit and never disconnects, so the scheduler never
    // reassigns. After ORPHAN_REAP_GRACE (5min), any Running Job the
    // scheduler doesn't consider busy is deleted.
    reap_orphan_running(&jobs_api, &jobs.items, &reaped, ctx, &name).await;

    // ---- Report terminations ----
    report_terminated_pods(ctx, &ns, &name).await;
    report_deadline_exceeded_jobs(ctx, &jobs.items).await;

    // ---- Status patch ----
    patch_job_pool_status(
        ctx,
        pool.status.as_ref(),
        &ns,
        &name,
        census.active,
        census.ready,
        ceiling.unwrap_or(queued as i32),
        scheduler_err.as_deref(),
    )
    .await?;

    Ok(Action::requeue(JOB_REQUEUE))
}

/// Per-drv spawn intents relevant to THIS pool.
///
/// D5: queries `GetSpawnIntents` filtered server-side by
/// `{kind=spec.kind, systems=spec.systems, features=spec.features}`.
/// The scheduler applies the same {system, feature} subset checks
/// `hard_filter` would (I-107/I-143/I-176/I-181), so every returned
/// intent is one this pool's workers could accept.
// r[impl ctrl.pool.fetcher-spawn-builtin]
async fn queued_for_pool(
    ctx: &Ctx,
    pool: &Pool,
) -> std::result::Result<Vec<SpawnIntent>, tonic::Status> {
    // I-176: `filter_features=true` even when `features` is empty: a
    // featureless pool then sees only featureless work.
    // `effective_features` (Fetcher → []) is the same chokepoint
    // `RIO_FEATURES` reads — keeps the spawn-decision query and the
    // spawned worker's capabilities derived from one value.
    let resp = admin_call(ctx.admin.clone().get_spawn_intents(
        rio_proto::types::GetSpawnIntentsRequest {
            kind: Some(super::executor_kind_to_proto(pool.spec.kind).into()),
            systems: pool.spec.systems.clone(),
            features: pod::effective_features(&pool.spec),
            filter_features: true,
        },
    ))
    .await?
    .into_inner();
    Ok(resp.intents)
}

/// Stable fingerprint of an intent's scheduler-decided placement:
/// `node_affinity` when non-empty (ADR-023 §13a — `k=v|k=v;k=v|...`
/// over sorted requirements per term, terms sorted), else the legacy
/// `node_selector` map (`k=v,k=v` over sorted keys). Empty → "". Only
/// the intent-supplied placement is fingerprinted (NOT the pool's base
/// selector that `build_executor_pod_spec` merges in) so drift
/// detection compares scheduler decisions, not pool config.
///
/// The §13a scheduler emits `node_selector: {}` (snapshot.rs:350), so
/// the legacy arm only fires on pre-§13a intents and on the
/// `node_affinity = []` FOD/feature-gated/cold-hw-table path.
fn selector_fingerprint(intent: &SpawnIntent) -> String {
    if !intent.node_affinity.is_empty() {
        let mut terms: Vec<String> = intent
            .node_affinity
            .iter()
            .map(|t| {
                let mut kv: Vec<_> = t
                    .match_expressions
                    .iter()
                    .map(|r| format!("{}={}", r.key, r.values.join("+")))
                    .collect();
                kv.sort_unstable();
                kv.join("|")
            })
            .collect();
        terms.sort_unstable();
        return terms.join(";");
    }
    let mut kv: Vec<_> = intent
        .node_selector
        .iter()
        .map(|(k, v)| format!("{k}={v}"))
        .collect();
    kv.sort_unstable();
    kv.join(",")
}

/// DNS-1123-safe deterministic suffix from `intent_id`. In production
/// `intent_id` is the FULL store path `/nix/store/{hash}-{name}.drv`
/// (translate.rs:`build_node` sets `drv_hash = drv_path`; snapshot.rs
/// sets `intent_id = drv_hash`). Strip the constant prefix so the
/// 12-char take lands on the nixbase32 hash (32⁵ ≈ 3.3e7× more
/// distinct values than the 4 hash chars left after `nixstore` ate 8).
/// The lowercase-alnum filter is belt-and-suspenders for the proto's
/// "opaque" contract; nixbase32 is already lowercase-alnum so it's a
/// no-op on the happy path.
fn intent_suffix(intent_id: &str) -> String {
    let s: String = intent_id
        .strip_prefix("/nix/store/")
        .unwrap_or(intent_id)
        .chars()
        .filter(|c| c.is_ascii_lowercase() || c.is_ascii_digit())
        .take(12)
        .collect();
    // Degenerate (all-filtered) — pad so the resulting Job name is
    // still valid DNS-1123 (no trailing hyphen).
    if s.is_empty() { "0".into() } else { s }
}

/// Delete Jobs whose name collides with an intent we are about to
/// spawn AND would block that spawn:
///
///   - **Terminal** (Complete/Failed): the deterministic `intent_
///     suffix` makes a finished Job for drv X block respawn of X for
///     [`JOB_TTL_SECS`]; clear that window so the immediate
///     `spawn_for_each` (or worst-case next tick) succeeds.
///     Background-propagation delete — terminal ⇒ `status.{succeeded,
///     failed} > 0` ⇒ the Job controller has already cleared the
///     pod's `batch.kubernetes.io/job-tracking` finalizer, so the
///     orphan-finalizer race does not apply and the Job object can
///     vanish before the pod.
///   - **Pending with stale selector**: the scheduler re-solved
///     (ICE-backoff spot→on-demand) but the prior Pending Job sits
///     unschedulable on the OLD selector and NameCollision-blocks the
///     new intent forever — `reap_excess_pending` won't catch it
///     because `pending == queued`. Foreground-propagation delete —
///     the pod's `job-tracking` finalizer is still live (same
///     reasoning as `reap_excess_pending`).
///
/// A Pending Job whose selector MATCHES the current intent is NOT
/// reaped — that's the intended NameCollision dedupe.
pub(super) async fn reap_stale_for_intents(
    jobs_api: &Api<Job>,
    existing: &[Job],
    intents: &[SpawnIntent],
    pool: &str,
    kind: ExecutorKind,
) -> HashSet<String> {
    let mut reaped = HashSet::new();
    let want: HashMap<String, String> = intents
        .iter()
        .map(|i| {
            (
                pod::job_name(pool, kind, &intent_suffix(&i.intent_id)),
                selector_fingerprint(i),
            )
        })
        .collect();
    if want.is_empty() {
        return reaped;
    }
    for j in existing {
        let Some(jn) = j.metadata.name.as_deref() else {
            continue;
        };
        let (params, why) = match want.get(jn) {
            // Not in the current intent set. Pending → orphan: the
            // intent left (cancel / completes-elsewhere / disconnect)
            // and this Job will never receive an assignment. Reap by
            // intent-membership HERE so the surplus is the orphan set,
            // not an arbitrary age-prefix — `select_excess_pending`'s
            // oldest-first reap would otherwise delete still-live Jobs
            // (losing in-flight Karpenter provisioning) while orphans
            // survive ≥1 extra tick. Running → leave alone (may hold
            // assignment; `reap_orphan_running` owns it). The
            // `want.is_empty()` early-return above is the fail-closed
            // gate (scheduler error → no orphan-reap).
            None if is_pending_job(j) && job_older_than(j, REAP_PENDING_GRACE) => {
                (DeleteParams::foreground(), "orphan-pending")
            }
            None => continue,
            Some(_) if !is_active_job(j) => (DeleteParams::background(), "terminal"),
            Some(want_sel)
                if is_pending_job(j)
                    && j.metadata
                        .annotations
                        .as_ref()
                        .and_then(|a| a.get(INTENT_SELECTOR_ANNOTATION))
                        .map(String::as_str)
                        != Some(want_sel.as_str()) =>
            {
                (DeleteParams::foreground(), "selector-drift")
            }
            Some(_) => continue,
        };
        match jobs_api.delete(jn, &params).await {
            Ok(_) => {
                info!(
                    pool, job = %jn, why,
                    "reaped stale Job blocking re-queued intent respawn"
                );
                reaped.insert(jn.to_owned());
            }
            Err(e) if e.is_not_found() => {
                reaped.insert(jn.to_owned());
            }
            Err(e) => {
                warn!(
                    pool, job = %jn, why, error = %e,
                    "failed to reap stale Job; spawn will NameCollision \
                     this tick, retried next"
                );
            }
        }
    }
    reaped
}

/// Build a K8s Job for one ephemeral worker pod.
///
/// Job-specific settings:
///   - `restartPolicy: Never` — if the worker crashes, the Job goes
///     Failed. The SCHEDULER owns retry.
///   - `backoffLimit: 0` — same reasoning. One attempt.
///   - `ttlSecondsAfterFinished: 600` — K8s TTL controller reaps.
///   - `activeDeadlineSeconds` — backstop for hung builds + wrong-
///     pool spawns.
// r[impl ctrl.pool.ephemeral]
// r[impl ctrl.ephemeral.intent-deadline]
#[allow(clippy::too_many_arguments)]
pub(super) fn build_job(
    pool: &Pool,
    oref: OwnerReference,
    scheduler: &UpstreamAddrs,
    store: &UpstreamAddrs,
    hw_config: &HwClassConfig,
    intent: &SpawnIntent,
    executor_token: Option<&str>,
    hw_sampled: &HwSampledCache,
    hw_bench_mem_floor: u64,
    gate_active: bool,
) -> Result<Job> {
    let pool_name = pool.name_any();
    // Suffix derives from `intent_id` so a re-polled still-Ready
    // intent re-creates the SAME Job name and the apiserver's
    // NameCollision dedupes.
    let suffix = intent_suffix(&intent.intent_id);
    let job_name = pod::job_name(&pool_name, pool.spec.kind, &suffix);
    let mut pod_spec = pod::build_executor_pod_spec(pool, scheduler, store, hw_config);
    apply_intent_resources(&mut pod_spec, pool, intent, hw_config);
    // r[impl ctrl.nodeclaim.priority-bucket]
    // §13b: route via the second kube-scheduler so MostAllocated bin-
    // packing matches `ffd::simulate`'s prediction, and bucket by
    // `⌊log₂ c*⌋` so largest-first holds at bind. Builder-only AND
    // gate-active only: fetcher pods land on the dedicated
    // `rio.build/node-role=fetcher` pool (the placeable gate doesn't
    // cover FOD nodes); when `gate_active=false` (NodeClaim CRD
    // absent — k3s VM tests without Karpenter) `kube-build-scheduler`
    // isn't deployed, so the pod would sit Pending forever.
    if gate_active && pool.spec.kind == ExecutorKind::Builder {
        pod_spec.scheduler_name = Some(KUBE_BUILD_SCHEDULER.into());
        pod_spec.priority_class_name = Some(format!(
            "{PRIORITY_CLASS_PREFIX}{}",
            priority_bucket(intent.cores)
        ));
    }
    // r[impl sec.executor.identity-token+2]
    // Pass the scheduler-signed token (from `MintExecutorTokens`, NOT
    // `SpawnIntent`) through verbatim so the builder presents it on
    // `BuildExecution` / `Heartbeat`. Per-intent (not per-Pool), so
    // it's appended here rather than in the static
    // `build_executor_pod_spec` env list. `None` in dev mode (or when
    // the mint RPC failed / drv left Ready between poll and mint) →
    // builder omits the header → scheduler permissive in dev mode,
    // rejects under HMAC mode → pod idle-exits, next tick re-spawns.
    if let Some(tok) = executor_token.filter(|t| !t.is_empty())
        && let Some(c) = pod_spec.containers.first_mut()
    {
        c.env
            .get_or_insert_with(Vec::new)
            .push(pod::env("RIO_EXECUTOR_TOKEN", tok));
    }
    let mut job = ephemeral_job(
        job_name,
        pool.namespace(),
        oref,
        pod::executor_labels(pool),
        ephemeral_deadline(intent),
        pod_spec,
    );
    // r[impl ctrl.pool.hw-bench-needed+2]
    // ADR-023 §13a bench gate: (a) `mem ≥ hw_bench_mem_floor` so
    // STREAM's ~4.6 GiB working set cannot OOM a `preferLocalBuild`/
    // fetcher pod; AND (b) any `h ∈ A` has < `trust_threshold` distinct
    // tenants in some K=3 dimension. The actual `h` is fixed only at
    // kube-scheduler bind, so the create-time check is over the whole
    // `A` — over-benches at most until every `h ∈ A` reaches the floor.
    let bench_needed = intent.mem_bytes >= hw_bench_mem_floor
        && hw_sampled.any_under_threshold(hw_classes_in(intent));
    // Stamp `rio.build/intent-id` on the pod template so the builder
    // reads it via downward-API → `RIO_INTENT_ID` → heartbeat →
    // scheduler matches the pod to its pre-computed assignment.
    let pod_anns = job
        .spec
        .as_mut()
        .and_then(|s| s.template.metadata.as_mut())
        .and_then(|m| m.annotations.as_mut())
        .expect("ephemeral_job sets template.metadata.annotations");
    pod_anns.insert(INTENT_ID_ANNOTATION.into(), intent.intent_id.clone());
    pod_anns.insert(HW_BENCH_NEEDED_ANNOTATION.into(), bench_needed.to_string());
    // Stamp the selector fingerprint on the JOB metadata (not pod
    // template) so `reap_stale_for_intents` can compare without
    // dereferencing `spec.template`.
    job.metadata
        .annotations
        .get_or_insert_with(BTreeMap::new)
        .insert(
            INTENT_SELECTOR_ANNOTATION.into(),
            selector_fingerprint(intent),
        );
    Ok(job)
}

/// Stamp scheduler-computed `(cores, mem, disk)` onto the executor
/// container's `resources` and the overlay emptyDir's `sizeLimit`.
///
/// `requests == limits` (hard caps, no burst) — ADR-023 §sizing-model.
/// Quantities rendered as raw byte counts (no SI suffix): k8s parses
/// bare integers as base-unit and they roundtrip exactly.
///
/// `ephemeral-storage` = `disk_bytes × headroom` (overlay writes, from
/// the SLA model's prjquota fit, plus the scheduler-computed
/// variance-aware cushion) + the per-pool FUSE cache budget (input
/// closure, NOT captured by `disk_p90`) + log/scratch headroom. BOTH
/// addends are the SAME values that set the `overlays` / `fuse-cache`
/// emptyDir sizeLimits, so kubelet's pod-level sum (writable-layer +
/// logs + disk-backed emptyDirs) cannot exceed the limit before a
/// volume-level limit fires. Budgeting bare `disk_bytes` (1.0×) here
/// while the overlay sizeLimit is `headroom×` made the headroom
/// unreachable — pods evicted at ≈p90 instead of `headroom×p90`.
fn apply_intent_resources(
    pod_spec: &mut PodSpec,
    pool: &Pool,
    i: &SpawnIntent,
    hw: &HwClassConfig,
) {
    let headroom = intent_headroom(i);
    let overlay_limit = (i.disk_bytes as f64 * headroom) as u64;
    let ephemeral = pod_ephemeral_request(i.disk_bytes, headroom, pod::fuse_cache_bytes(pool));
    let map: BTreeMap<String, Quantity> = BTreeMap::from([
        ("cpu".into(), Quantity(i.cores.to_string())),
        ("memory".into(), Quantity(i.mem_bytes.to_string())),
        ("ephemeral-storage".into(), Quantity(ephemeral.to_string())),
    ]);
    let container = pod_spec
        .containers
        .first_mut()
        .expect("build_executor_pod_spec emits exactly one container");
    container.resources = Some(ResourceRequirements {
        requests: Some(map.clone()),
        limits: Some(map),
        ..Default::default()
    });

    // Couple the worker's `daemon_timeout` to the per-intent K8s
    // `activeDeadlineSeconds`: worker fires `WORKER_DEADLINE_SLACK_SECS`
    // BEFORE K8s SIGKILLs, so `CompletionReport{TimedOut}` (primary
    // path) carries telemetry and reaches `handle_timeout_failure`'s
    // cap-check; `DeadlineExceeded` stays the wedged-worker backstop
    // per `r[sched.termination.deadline-exceeded+2]`.
    // `ephemeral_deadline` floors at 180 so `− 90` never underflows
    // the `.max(60)` clamp into a tie.
    let worker_timeout = (ephemeral_deadline(i) - WORKER_DEADLINE_SLACK_SECS).max(60);
    let env = container.env.get_or_insert_with(Vec::new);
    env.push(pod::env(
        "RIO_DAEMON_TIMEOUT_SECS",
        &worker_timeout.to_string(),
    ));
    // r[impl ctrl.pool.hw-bench-needed+2]
    // Downward-API env var for `rio.build/hw-bench-needed`. The
    // annotation is stamped at pod-CREATE time by `build_job` (above on
    // the call stack), so the env-var form's resolve-once-at-container-
    // create is race-free here — unlike `rio.build/hw-class` which is
    // stamped after-bind by `run_pod_annotator` and so MUST use the
    // volume form. Absent annotation (recovery path) → kubelet resolves
    // to "" → `figment` → `hw_bench_needed: false` (fail-closed).
    env.push(pod::env_from_field(
        "RIO_HW_BENCH_NEEDED",
        &format!("metadata.annotations['{HW_BENCH_NEEDED_ANNOTATION}']"),
    ));

    // ADR-023 phase-13: per-(band, cap) targeting. Merge into the
    // existing nodeSelector; intent keys win on collision.
    if !i.node_selector.is_empty() {
        let ns = pod_spec.node_selector.get_or_insert_with(BTreeMap::new);
        for (k, v) in &i.node_selector {
            ns.insert(k.clone(), v.clone());
        }
    }
    // r[impl ctrl.pool.node-affinity-from-intent]
    // ADR-023 §13a: OR-of-ANDs over `(h, cap)` cells. `required…
    // ignored…` so a Pending pod whose admissible set narrows after
    // create stays Pending until `reap_stale_for_intents` notices the
    // fingerprint drift; a RUNNING pod is never evicted on a re-solve.
    // `build_executor_pod_spec` sets no affinity, so `get_or_insert_
    // default` is currently a plain insert; written as a merge so a
    // future pod-level pod-anti-affinity (or §13b's `preferred…`
    // soft-spread) survives.
    if !i.node_affinity.is_empty() {
        pod_spec
            .affinity
            .get_or_insert_with(Default::default)
            .node_affinity = Some(NodeAffinity {
            required_during_scheduling_ignored_during_execution: Some(NodeSelector {
                node_selector_terms: i.node_affinity.iter().map(pod::proto_term_to_k8s).collect(),
            }),
            ..Default::default()
        });
    }

    // r[impl ctrl.pool.intent-tolerations]
    // §13d toleration axis (r31 bug_020): the intent's `node_affinity`
    // (stamped above) pins the pod to nodes carrying the hwClass
    // labels — incl. taint-paired keys like `rio.build/kvm`. The
    // affinity producer (scheduler `cells_to_selector_terms`) and the
    // taint producer (`cover::build_nodeclaim`) both read
    // `[sla.hw_classes.$h]`; this derives the matching tolerations
    // from the SAME map (`HwClassConfig.taints_for(h)`) so a future
    // tainted hwClass (gpu, secure-boot) routes its toleration
    // automatically. (`pod::wants_metal` covers the pool-static
    // *toleration* path for `hw_class_names=[]` cold-start intents;
    // this covers the intent-affinity path. There is no pool-static
    // *nodeSelector* path — r33 bug_002 deleted it. Restrictive
    // placement is `intent.node_affinity` only.) Append-dedup so the
    // operator-set `rio.build/builder` toleration and the pool-static
    // kvm toleration both survive without duplication.
    let mut intent_tols: Vec<Toleration> = Vec::new();
    for h in &i.hw_class_names {
        for t in hw.taints_for(h) {
            let tol = Toleration {
                key: Some(t.key),
                operator: Some("Equal".into()),
                value: Some(t.value),
                effect: Some(t.effect),
                ..Default::default()
            };
            if !intent_tols.contains(&tol) {
                intent_tols.push(tol);
            }
        }
    }
    if !intent_tols.is_empty() {
        let pod_t = pod_spec.tolerations.get_or_insert_with(Vec::new);
        for t in intent_tols {
            if !pod_t.contains(&t) {
                pod_t.push(t);
            }
        }
    }

    // Overlay emptyDir sizeLimit — same `overlay_limit` used as the
    // overlay addend above so kubelet's pod-level sum cannot fire
    // before the volume cap.
    if let Some(volumes) = pod_spec.volumes.as_mut() {
        for v in volumes.iter_mut() {
            if v.name == "overlays"
                && let Some(ed) = v.empty_dir.as_mut()
            {
                ed.size_limit = Some(Quantity(overlay_limit.to_string()));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fixtures::{test_pool, test_sched_addrs, test_store_addrs};

    fn intent(id: &str) -> SpawnIntent {
        SpawnIntent {
            intent_id: id.into(),
            ..Default::default()
        }
    }

    /// `build_job` wrapper for tests that don't exercise the §13a
    /// hw-bench gate. Empty cache + 0 floor → `bench_needed = false`
    /// (vacuous on `A = ∅`). Default `HwClassConfig` ⇔ `wants_metal`
    /// falls back to the literal `kvm` feature and `apply_intent_
    /// resources` adds no per-intent tolerations (`taints_for(h)` empty).
    fn job(pool: &Pool, i: &SpawnIntent) -> Job {
        build_job(
            pool,
            crate::fixtures::oref(pool),
            &test_sched_addrs(),
            &test_store_addrs(),
            &HwClassConfig::default(),
            i,
            None,
            &HwSampledCache::default(),
            0,
            true,
        )
        .unwrap()
    }

    /// Built Job has all the load-bearing settings. If any of these
    /// drift, the Job reconciler breaks silently:
    ///   - restartPolicy != Never → K8s rejects the Job on create
    ///   - backoffLimit > 0 → K8s retries on crash, scheduler ALSO
    ///     retries → duplicate build
    ///   - ttlSecondsAfterFinished missing → completed Jobs
    ///     accumulate forever
    ///   - ownerReference missing → Pool delete leaves orphan Jobs
    // r[verify ctrl.pool.ephemeral]
    #[test]
    fn job_spec_load_bearing_fields() {
        let pool = test_pool("eph-pool", ExecutorKind::Builder);
        let job = job(&pool, &intent("abc123"));

        let orefs = job.metadata.owner_references.as_ref().unwrap();
        assert_eq!(orefs[0].kind, "Pool");
        assert_eq!(orefs[0].controller, Some(true));

        let labels = job.metadata.labels.as_ref().unwrap();
        assert_eq!(labels.get("rio.build/pool"), Some(&"eph-pool".to_string()));

        let spec = job.spec.as_ref().unwrap();
        assert_eq!(spec.backoff_limit, Some(0), "K8s must not retry");
        assert_eq!(spec.parallelism, Some(1), "one pod per Job");
        assert_eq!(
            spec.ttl_seconds_after_finished,
            Some(JOB_TTL_SECS),
            "completed Jobs must auto-reap"
        );
        // r[verify ctrl.ephemeral.intent-deadline]
        assert_eq!(
            spec.active_deadline_seconds,
            Some(180),
            "activeDeadlineSeconds backstop (proto-default 0 → 180s floor)"
        );

        let pod_anns = spec
            .template
            .metadata
            .as_ref()
            .and_then(|m| m.annotations.as_ref())
            .expect("pod template must have annotations");
        assert_eq!(
            pod_anns
                .get("karpenter.sh/do-not-disrupt")
                .map(String::as_str),
            Some("true"),
            "I-126: ephemeral pods must opt out of karpenter disruption"
        );
        assert_eq!(
            pod_anns.get(INTENT_ID_ANNOTATION),
            Some(&"abc123".to_string()),
            "intent_id annotation feeds RIO_INTENT_ID downward-API"
        );

        let pod_spec = spec.template.spec.as_ref().unwrap();
        assert_eq!(
            pod_spec.restart_policy.as_deref(),
            Some("Never"),
            "K8s rejects Jobs with restartPolicy=Always"
        );
        assert!(
            pod_spec.affinity.is_none() && pod_spec.topology_spread_constraints.is_none(),
            "I-090: ephemeral Jobs bin-pack (no anti-affinity/spread)"
        );

        let env = pod_spec.containers[0].env.as_ref().unwrap();
        assert!(
            env.iter().any(|e| e.name == "RIO_SCHEDULER__ADDR"),
            "build_executor_pod_spec env should be preserved"
        );
    }

    /// Job name derives from intent_id so re-poll dedupes.
    #[test]
    fn job_name_format() {
        let pool = test_pool("eph-pool", ExecutorKind::Builder);
        let job = job(&pool, &intent("0a1b2c3d4f5g6h7i"));
        assert_eq!(
            job.metadata.name.as_deref(),
            Some("rio-builder-eph-pool-0a1b2c3d4f5g")
        );
    }

    // r[verify ctrl.ephemeral.intent-deadline]
    /// D7: `SpawnIntent.deadline_secs` propagates verbatim;
    /// proto-default 0 floors to 180.
    #[test]
    fn intent_deadline_propagates_to_job_spec() {
        let i = SpawnIntent {
            intent_id: "abc123def456".into(),
            deadline_secs: 240,
            ..Default::default()
        };
        assert_eq!(ephemeral_deadline(&i), 240);
        assert_eq!(ephemeral_deadline(&intent("abc")), 180, "0 → 180s floor");
    }

    /// §Simulator-shares-accounting: the `(c,m,d)` triple
    /// `apply_intent_resources` stamps on `pod.resources.requests`
    /// MUST equal `intent_pod_footprint(i, fuse)` — the same triple
    /// FFD fit-checks against. A 5th FFD-divergence (raw disk_bytes,
    /// missing headroom, …) would surface here as the executable
    /// guarantee, not as accounting drift in production.
    #[test]
    fn footprint_matches_stamped_requests() {
        let pool = test_pool("p", ExecutorKind::Builder);
        let i = SpawnIntent {
            intent_id: "x".into(),
            cores: 8,
            mem_bytes: 16 * (1 << 30),
            disk_bytes: 40 * (1 << 30),
            disk_headroom_factor: Some(1.7),
            ..Default::default()
        };
        let j = job(&pool, &i);
        let spec = j
            .spec
            .as_ref()
            .and_then(|s| s.template.spec.as_ref())
            .unwrap();
        let req = spec.containers[0]
            .resources
            .as_ref()
            .and_then(|r| r.requests.as_ref())
            .unwrap();
        let q = |k: &str| req[k].0.parse::<u64>().unwrap();
        // mb_035: the production input source for FFD/cover_deficit is
        // `cfg.fuse_cache_bytes` (= `BUILDER_FUSE_CACHE`), NOT
        // `pod::fuse_cache_bytes(&pool)` directly. Passing the latter to
        // both sides was vacuous — it couldn't catch the two values
        // diverging. With Builder pools single-sourced, both ARE the
        // OnceLock; assert that's what the stamp side also reads.
        let cfg_fuse = *pod::BUILDER_FUSE_CACHE
            .get()
            .unwrap_or(&pod::BUILDER_FUSE_CACHE_BYTES);
        assert_eq!(
            pod::fuse_cache_bytes(&pool),
            cfg_fuse,
            "Builder pool fuse_cache_bytes single-sourced from BUILDER_FUSE_CACHE"
        );
        let (fc, fm, fd) = intent_pod_footprint(&i, cfg_fuse);
        assert_eq!(q("cpu"), u64::from(fc));
        assert_eq!(q("memory"), fm);
        assert_eq!(q("ephemeral-storage"), fd);
    }

    /// mb_022: a Builder Pool that sets `fuseCacheBytes` (pre-CEL CR)
    /// is silently ignored at the value-read site — `fuse_cache_bytes`
    /// reads `BUILDER_FUSE_CACHE` regardless. The Warning event is
    /// `DEGRADE_CHECKS::BuilderFuseCacheBytesIgnored` (covered in
    /// `disruption_tests::degrade_builder_fuse_cache_ignored`); this
    /// asserts the silent-ignore half.
    #[test]
    fn builder_pool_ignores_fuse_cache_override() {
        let mut p = test_pool("p", ExecutorKind::Builder);
        p.spec.fuse_cache_bytes = Some(100 * (1 << 30));
        let cfg_fuse = *pod::BUILDER_FUSE_CACHE
            .get()
            .unwrap_or(&pod::BUILDER_FUSE_CACHE_BYTES);
        assert_eq!(
            pod::fuse_cache_bytes(&p),
            cfg_fuse,
            "Builder ignores spec.fuseCacheBytes — single-sourced from BUILDER_FUSE_CACHE"
        );
        // Fetcher may override.
        let mut f = test_pool("f", ExecutorKind::Fetcher);
        f.spec.fuse_cache_bytes = Some(100 * (1 << 30));
        assert_eq!(pod::fuse_cache_bytes(&f), 100 * (1 << 30));
    }

    /// `apply_intent_resources` injects `RIO_DAEMON_TIMEOUT_SECS =
    /// activeDeadlineSeconds − 90` so the worker times out before K8s
    /// SIGKILLs. Regression: a fitted `deadline_secs=15000` build with
    /// the old decoupled 7200s static default looped `TimedOut` at
    /// 7200s while only the K8s side doubled.
    #[test]
    fn build_job_daemon_timeout_couples_to_intent_deadline() {
        let pool = test_pool("p", ExecutorKind::Builder);
        for (deadline, want) in [(15000, "14910"), (240, "150"), (0, "90"), (120, "90")] {
            let i = SpawnIntent {
                intent_id: "abc".into(),
                deadline_secs: deadline,
                ..Default::default()
            };
            let job = job(&pool, &i);
            let env = job
                .spec
                .as_ref()
                .and_then(|s| s.template.spec.as_ref())
                .map(|p| &p.containers[0])
                .and_then(|c| c.env.as_deref())
                .unwrap();
            let envs = crate::fixtures::env_map(env);
            assert_eq!(
                envs.get("RIO_DAEMON_TIMEOUT_SECS"),
                Some(&want),
                "deadline_secs={deadline} → daemon_timeout={want} \
                 (activeDeadlineSeconds − {WORKER_DEADLINE_SLACK_SECS}; \
                 ephemeral_deadline floored at 180)"
            );
            assert_eq!(
                env.iter()
                    .filter(|e| e.name == "RIO_DAEMON_TIMEOUT_SECS")
                    .count(),
                1,
                "exactly one entry"
            );
        }
    }

    /// `selector_fingerprint` is deterministic over key/term order. Empty
    /// → "". `build_job` stamps it on Job metadata.annotations so
    /// `reap_stale_for_intents` can compare without dereferencing
    /// `spec.template`.
    #[test]
    fn build_job_stamps_selector_fingerprint() {
        use rio_proto::types::{NodeSelectorRequirement, NodeSelectorTerm};
        let term = |kv: &[(&str, &str)]| NodeSelectorTerm {
            match_expressions: kv
                .iter()
                .map(|(k, v)| NodeSelectorRequirement {
                    key: (*k).into(),
                    operator: "In".into(),
                    values: vec![(*v).into()],
                })
                .collect(),
        };
        let a = SpawnIntent {
            node_affinity: vec![
                term(&[
                    ("karpenter.sh/capacity-type", "spot"),
                    ("rio.build/hw-band", "mid"),
                ]),
                term(&[
                    ("rio.build/hw-band", "hi"),
                    ("karpenter.sh/capacity-type", "spot"),
                ]),
            ],
            ..Default::default()
        };
        let b = SpawnIntent {
            node_affinity: vec![
                term(&[
                    ("karpenter.sh/capacity-type", "spot"),
                    ("rio.build/hw-band", "hi"),
                ]),
                term(&[
                    ("rio.build/hw-band", "mid"),
                    ("karpenter.sh/capacity-type", "spot"),
                ]),
            ],
            ..Default::default()
        };
        assert_eq!(
            selector_fingerprint(&a),
            selector_fingerprint(&b),
            "deterministic over both per-term key order and term order"
        );
        // Legacy `node_selector` arm still works for non-§13a intents.
        let legacy = SpawnIntent {
            node_selector: [
                ("karpenter.sh/capacity-type".into(), "spot".into()),
                ("rio.build/hw-band".into(), "mid".into()),
            ]
            .into(),
            ..Default::default()
        };
        assert_eq!(
            selector_fingerprint(&legacy),
            "karpenter.sh/capacity-type=spot,rio.build/hw-band=mid"
        );
        assert_eq!(selector_fingerprint(&SpawnIntent::default()), "");

        let pool = test_pool("p", ExecutorKind::Builder);
        let i = SpawnIntent {
            intent_id: "abc".into(),
            ..a
        };
        let job = job(&pool, &i);
        assert_eq!(
            job.metadata
                .annotations
                .as_ref()
                .and_then(|a| a.get(INTENT_SELECTOR_ANNOTATION))
                .map(String::as_str),
            Some(selector_fingerprint(&i).as_str()),
        );
    }

    /// bug_074: fetcher overlay emptyDir is disk-backed (NOT
    /// `medium: Memory`). Under ADR-023 `limits.memory` is RSS-only
    /// and `disk_bytes` budgets `ephemeral-storage`; a tmpfs overlay
    /// charged the unpack against memory → OOM while the disk
    /// reservation sat unused.
    #[test]
    fn fetcher_overlay_is_disk_backed() {
        let pool = test_pool("f", ExecutorKind::Fetcher);
        let i = SpawnIntent {
            intent_id: "abc".into(),
            disk_bytes: 8 << 30,
            ..Default::default()
        };
        let job = job(&pool, &i);
        let overlay = job
            .spec
            .as_ref()
            .and_then(|s| s.template.spec.as_ref())
            .and_then(|p| p.volumes.as_ref())
            .and_then(|v| v.iter().find(|v| v.name == "overlays"))
            .and_then(|v| v.empty_dir.as_ref())
            .expect("fetcher pod has overlays emptyDir");
        assert_eq!(
            overlay.medium, None,
            "fetcher overlay must be disk-backed so disk_bytes budgets \
             ephemeral-storage and quota::current_bytes() sees prjquota"
        );
        assert!(overlay.size_limit.is_some(), "sizeLimit still applied");
    }

    /// `intent_suffix` is deterministic and DNS-1123-safe.
    #[test]
    fn intent_suffix_deterministic_and_dns_safe() {
        let h = "0a1b2c3d4f5g6h7i8j9k0l1m2n3p4q5r";
        assert_eq!(intent_suffix(h), "0a1b2c3d4f5g");
        assert_eq!(intent_suffix(h), intent_suffix(h), "deterministic");
        assert_eq!(intent_suffix("FOO-bar.baz/9"), "barbaz9");
        assert_eq!(intent_suffix("---"), "0");
    }

    /// Production `intent_id` is the full store path, not a bare hash
    /// (translate.rs:`build_node` → `drv_hash = drv_path`). Without
    /// the prefix-strip the lowercase-alnum filter eats 8 of 12 chars
    /// on the constant `"nixstore"`, leaving 4 hash chars → ~38%
    /// collision at 1000 concurrent. These two paths share the first
    /// 4 hash chars and MUST produce distinct suffixes.
    #[test]
    fn intent_suffix_distinct_for_store_paths_sharing_prefix() {
        let a = "/nix/store/amnhr5p1w6gmjb7bynh7vxdfjs8x3kr2-firefox-149.0.drv";
        let b = "/nix/store/amnhqqy03c4k8f2sgh5j7nv9wp1x6r8z-glibc-2.40.drv";
        assert_eq!(intent_suffix(a), "amnhr5p1w6gm");
        assert_eq!(intent_suffix(b), "amnhqqy03c4k");
        assert_ne!(
            intent_suffix(a),
            intent_suffix(b),
            "store paths with shared 4-char hash prefix must not collide"
        );
        // Bare-hash inputs (controller unit tests) still work — strip
        // is `unwrap_or(intent_id)`.
        assert_eq!(
            intent_suffix("amnhr5p1w6gmjb7bynh7vxdfjs8x3kr2"),
            intent_suffix(a),
            "bare hash and full path produce same suffix"
        );
    }

    /// ADR-023: `build_job` stamps the scheduler-computed resources
    /// onto the executor container and the overlay emptyDir.
    // r[verify sched.sla.disk-reaches-ephemeral-storage]
    #[test]
    fn build_job_with_intent_computed_resources() {
        const GI: u64 = 1 << 30;
        let pool = test_pool("eph-pool", ExecutorKind::Builder);
        let i = SpawnIntent {
            intent_id: "i-abc".into(),
            cores: 8,
            mem_bytes: 16 * GI,
            disk_bytes: 40 * GI,
            node_selector: [
                ("rio.build/hw-band".into(), "mid".into()),
                ("karpenter.sh/capacity-type".into(), "spot".into()),
            ]
            .into(),
            ..Default::default()
        };
        let job = job(&pool, &i);

        let tmpl = &job.spec.as_ref().unwrap().template;
        assert_eq!(
            job.metadata.name.as_deref(),
            Some("rio-builder-eph-pool-iabc")
        );

        let pod_spec = tmpl.spec.as_ref().unwrap();
        let res = pod_spec.containers[0].resources.as_ref().unwrap();
        let req = res.requests.as_ref().unwrap();
        assert_eq!(req["cpu"], Quantity("8".into()));
        assert_eq!(req["memory"], Quantity((16 * GI).to_string()));
        assert_eq!(
            req["ephemeral-storage"],
            Quantity(((60 + 8 + 1) * GI).to_string()),
            "disk_bytes×OVERLAY_HEADROOM_FALLBACK + BUILDER_FUSE_CACHE_BYTES + LOG_BUDGET_BYTES"
        );
        assert_eq!(
            res.limits.as_ref(),
            Some(req),
            "limits == requests (hard caps, no burst)"
        );

        let overlay = pod_spec
            .volumes
            .as_ref()
            .unwrap()
            .iter()
            .find(|v| v.name == "overlays")
            .unwrap();
        assert_eq!(
            overlay.empty_dir.as_ref().unwrap().size_limit,
            Some(Quantity((60 * GI).to_string()))
        );
    }

    /// ADR-023 §sizing F5: a low-`n_eff` fit produces a wider
    /// `disk_headroom_factor` than a high-`n_eff` fit, and that flows
    /// through to a larger pod `ephemeral-storage` request + overlay
    /// sizeLimit. `headroom(n)` = `1.25 + 0.7/√n` is monotone
    /// decreasing in `n`, so cold/noisy keys get more cushion.
    #[test]
    fn disk_headroom_factor_widens_ephemeral_request() {
        const GI: u64 = 1 << 30;
        let pool = test_pool("p", ExecutorKind::Builder);
        let mk = |h: Option<f64>| {
            let i = SpawnIntent {
                intent_id: "abc".into(),
                disk_bytes: 40 * GI,
                disk_headroom_factor: h,
                ..Default::default()
            };
            let job = job(&pool, &i);
            let pod_spec = job
                .spec
                .as_ref()
                .and_then(|s| s.template.spec.as_ref())
                .unwrap();
            let eph: u64 = pod_spec.containers[0]
                .resources
                .as_ref()
                .and_then(|r| r.limits.as_ref())
                .map(|l| l["ephemeral-storage"].0.parse().unwrap())
                .unwrap();
            let overlay: u64 = pod_spec
                .volumes
                .as_ref()
                .and_then(|v| v.iter().find(|v| v.name == "overlays"))
                .and_then(|v| v.empty_dir.as_ref())
                .and_then(|e| e.size_limit.as_ref())
                .map(|q| q.0.parse().unwrap())
                .unwrap();
            (eph, overlay)
        };
        // headroom(100)≈1.32, headroom(3)≈1.65 — scheduler-side values.
        let (eph_hi, ov_hi) = mk(Some(1.32));
        let (eph_lo, ov_lo) = mk(Some(1.65));
        assert!(
            eph_lo > eph_hi && ov_lo > ov_hi,
            "low-n_eff (h=1.65) must request MORE disk than high-n_eff \
             (h=1.32); got eph {eph_lo} vs {eph_hi}, overlay {ov_lo} vs {ov_hi}"
        );
        // Absent → fallback 1.5×; 0.0 → also fallback (proto default).
        let (eph_none, _) = mk(None);
        let (eph_zero, _) = mk(Some(0.0));
        let (eph_fb, _) = mk(Some(OVERLAY_HEADROOM_FALLBACK));
        assert_eq!(eph_none, eph_fb, "None → fallback");
        assert_eq!(eph_zero, eph_fb, "0.0 → fallback");
    }

    /// The `fuse-cache` emptyDir sizeLimit and the FUSE-cache addend in
    /// the container's `ephemeral-storage` limit MUST come from the
    /// same per-pool value. Kubelet sums disk-backed emptyDirs against
    /// the container limit, so a sizeLimit larger than the budget
    /// evicts on the pod-level limit before the volume cap — and
    /// `disk_p90` (overlay prjquota only) never learns the input-
    /// closure size, so every fresh drv_hash re-climbs the floor.
    ///
    /// Third arm: `PoolSpec.fuse_cache_bytes` override is honoured for
    /// BOTH sites — helm-rendered prod Pools set 50Gi, which would make
    /// every pod request ≥51Gi ephemeral-storage on small-disk nodes
    /// (k3s VM tests) without the override.
    #[test]
    fn fuse_cache_budget_matches_sizelimit() {
        const GI: u64 = 1 << 30;
        let builder_fuse = *pod::BUILDER_FUSE_CACHE
            .get()
            .unwrap_or(&pod::BUILDER_FUSE_CACHE_BYTES);
        for (kind, override_, expect) in [
            (ExecutorKind::Builder, None, builder_fuse),
            (ExecutorKind::Fetcher, None, pod::FETCHER_FUSE_CACHE_BYTES),
            // mb_035: Builder ignores PoolSpec override (single-sourced).
            (ExecutorKind::Builder, Some(4 * GI), builder_fuse),
            // Fetcher honours per-pool override.
            (ExecutorKind::Fetcher, Some(6 * GI), 6 * GI),
        ] {
            let mut pool = test_pool("p", kind);
            pool.spec.fuse_cache_bytes = override_;
            let i = SpawnIntent {
                intent_id: "abc".into(),
                disk_bytes: 5 * GI,
                ..Default::default()
            };
            let job = job(&pool, &i);
            let pod_spec = job
                .spec
                .as_ref()
                .and_then(|s| s.template.spec.as_ref())
                .unwrap();
            let fuse = pod_spec
                .volumes
                .as_ref()
                .and_then(|v| v.iter().find(|v| v.name == "fuse-cache"))
                .and_then(|v| v.empty_dir.as_ref())
                .and_then(|e| e.size_limit.as_ref())
                .expect("fuse-cache emptyDir has sizeLimit");
            assert_eq!(fuse, &Quantity(expect.to_string()), "{kind:?} sizeLimit");
            let eph = pod_spec.containers[0]
                .resources
                .as_ref()
                .and_then(|r| r.limits.as_ref())
                .map(|l| l["ephemeral-storage"].clone())
                .unwrap();
            let overlay_limit = ((5 * GI) as f64 * OVERLAY_HEADROOM_FALLBACK) as u64;
            assert_eq!(
                eph,
                Quantity((overlay_limit + expect + LOG_BUDGET_BYTES).to_string()),
                "{kind:?} ephemeral-storage budget must include the SAME \
                 fuse-cache bytes as the emptyDir sizeLimit"
            );
        }
    }

    /// Structural invariant: container `ephemeral-storage` limit ≥
    /// Σ(disk-backed emptyDir sizeLimits) + LOG_BUDGET. Kubelet sums
    /// all disk-backed emptyDirs against the container limit and
    /// evicts when the sum exceeds it — independent of per-volume
    /// sizeLimit. If the container limit is smaller, the per-volume
    /// caps are unreachable (the headroom cushion becomes phantom;
    /// pods evict at ≈p90 instead of `headroom×p90`).
    ///
    /// Invariant under any `disk_headroom_factor` value (ADR-023
    /// §sizing computes it scheduler-side as `headroom(n_eff)`).
    #[test]
    fn disk_backed_emptydir_sizelimits_fit_ephemeral_limit() {
        const GI: u64 = 1 << 30;
        for kind in [ExecutorKind::Builder, ExecutorKind::Fetcher] {
            let pool = test_pool("p", kind);
            let i = SpawnIntent {
                intent_id: "abc".into(),
                disk_bytes: 40 * GI,
                ..Default::default()
            };
            let job = job(&pool, &i);
            let pod_spec = job
                .spec
                .as_ref()
                .and_then(|s| s.template.spec.as_ref())
                .unwrap();
            let eph: u64 = pod_spec.containers[0]
                .resources
                .as_ref()
                .and_then(|r| r.limits.as_ref())
                .map(|l| l["ephemeral-storage"].0.parse().unwrap())
                .unwrap();
            // Sum every disk-backed emptyDir sizeLimit. `medium` unset
            // (default) = node disk; `medium=Memory` (tmpfs) doesn't
            // count against ephemeral-storage.
            let sum_sizelimits: u64 = pod_spec
                .volumes
                .iter()
                .flatten()
                .filter_map(|v| v.empty_dir.as_ref())
                .filter(|ed| ed.medium.as_deref() != Some("Memory"))
                .filter_map(|ed| ed.size_limit.as_ref())
                .map(|q| q.0.parse::<u64>().unwrap())
                .sum();
            assert!(
                eph >= sum_sizelimits + LOG_BUDGET_BYTES,
                "{kind:?}: ephemeral-storage limit {eph} < Σ(disk-backed \
                 emptyDir sizeLimits) {sum_sizelimits} + LOG_BUDGET — \
                 kubelet evicts before any volume cap fires"
            );
        }
    }

    // r[verify ctrl.nodeclaim.priority-bucket]
    /// `⌊log₂ c*⌋` clamped `[0, 9]`. The clamp is reachable only at the
    /// `SlaConfig` ceiling (`maxCores < 1024`); `cores = 0` (proto
    /// default) maps to bucket 0, not panic.
    #[test]
    fn priority_bucket_log2_floor() {
        for (cores, want) in [
            (0, 0),
            (1, 0),
            (2, 1),
            (3, 1),
            (4, 2),
            (17, 4),
            (511, 8),
            (512, 9),
            (1023, 9),
            (u32::MAX, 9),
        ] {
            assert_eq!(priority_bucket(cores), want, "cores={cores}");
        }
    }

    // r[verify ctrl.nodeclaim.priority-bucket]
    /// Builder pods get `schedulerName: kube-build-scheduler` +
    /// `priorityClassName: rio-builder-prio-{⌊log₂ c*⌋}`. Fetcher pods
    /// NEVER get them (placeable gate doesn't cover the dedicated
    /// fetcher pool).
    #[test]
    fn build_job_stamps_kube_build_scheduler_and_priority() {
        let i = SpawnIntent {
            intent_id: "abc".into(),
            cores: 17,
            ..Default::default()
        };
        let pod = |j: &Job| j.spec.as_ref().unwrap().template.spec.clone().unwrap();

        let builder = test_pool("p", ExecutorKind::Builder);
        let on = pod(&job(&builder, &i));
        assert_eq!(on.scheduler_name.as_deref(), Some(KUBE_BUILD_SCHEDULER));
        assert_eq!(
            on.priority_class_name.as_deref(),
            Some("rio-builder-prio-4"),
            "⌊log₂ 17⌋ = 4"
        );

        let fetcher = test_pool("f", ExecutorKind::Fetcher);
        let f = pod(&job(&fetcher, &i));
        assert_eq!(
            f.scheduler_name, None,
            "fetcher pods stay on default scheduler"
        );
        assert_eq!(f.priority_class_name, None);
    }

    // r[verify ctrl.nodeclaim.placeable-gate+2]
    /// `PlaceableGate::retain` filters to the FFD-placed-on-Registered
    /// set; unarmed gate clears + returns `false` so `queued_known =
    /// None` (fail-closed reap).
    #[test]
    fn placeable_gate_retain_semantics() {
        use crate::reconcilers::nodeclaim_pool::PlaceableGate;
        let mk = |ids: &[&str]| -> Vec<SpawnIntent> { ids.iter().map(|i| intent(i)).collect() };

        // Armed → filter to set, armed=true.
        let gate = PlaceableGate::from_ids(["a", "c", "z"]);
        let mut v = mk(&["a", "b", "c"]);
        assert!(gate.retain(&mut v));
        let ids: Vec<&str> = v.iter().map(|i| i.intent_id.as_str()).collect();
        assert_eq!(ids, vec!["a", "c"]);

        // Unarmed → clear, armed=false.
        let gate = PlaceableGate::unarmed();
        let mut v = mk(&["a", "b"]);
        assert!(!gate.retain(&mut v), "unarmed → false (fail-closed reap)");
        assert!(v.is_empty());
    }

    // r[verify ctrl.nodeclaim.placeable-gate+2]
    /// The spawn-intent fan-out close in unit-test form: 1226 Ready
    /// intents, FFD placed 9 on Registered nodes → only 9 survive the
    /// gate. Pre-B12 (`ready` retain) all 1226 would mint Pending Jobs;
    /// post-B12 the Job count is bounded by Registered-node capacity.
    #[test]
    fn placeable_gate_bounds_spawn_intent_fan_out() {
        use crate::reconcilers::nodeclaim_pool::PlaceableGate;
        let mut intents: Vec<SpawnIntent> = (0..1226)
            .map(|k| SpawnIntent {
                intent_id: format!("i{k:04}"),
                ready: Some(true),
                ..Default::default()
            })
            .collect();
        // FFD placed 9 on Registered nodes (arbitrary subset).
        let placed = [
            "i0000", "i0042", "i0137", "i0511", "i0512", "i0777", "i0999", "i1000", "i1225",
        ];
        let gate = PlaceableGate::from_ids(placed);
        assert!(gate.retain(&mut intents));
        assert_eq!(
            intents.len(),
            placed.len(),
            "1226 Ready intents → {} placeable Jobs (bounded by Registered-node \
             capacity, not Ready-set size)",
            placed.len()
        );
        let survived: HashSet<&str> = intents.iter().map(|i| i.intent_id.as_str()).collect();
        for id in placed {
            assert!(survived.contains(id), "{id} survived");
        }
    }

    /// `placeable_channel()` end-to-end: publish from the producer side
    /// (what `reconcile_once` does after `ffd::simulate`) filtering
    /// `in_flight = false`, then read via the gate. Proves the
    /// `Placement → intent_id` projection and the `Registered`-only
    /// filter are wired the same way at both ends.
    #[test]
    fn placeable_channel_publish_filters_in_flight() {
        use crate::reconcilers::nodeclaim_pool::{Placement, placeable_channel};
        let (tx, gate) = placeable_channel();
        // Unarmed until first publish.
        let mut v = vec![intent("x")];
        assert!(!gate.retain(&mut v));

        let placeable: Vec<Placement> = vec![
            (intent("on-reg"), "n1".into(), false),
            (intent("on-inflight"), "n2".into(), true),
            (intent("on-reg-2"), "n3".into(), false),
        ];
        let on_registered: HashSet<String> = placeable
            .iter()
            .filter(|(_, _, in_flight)| !in_flight)
            .map(|(i, _, _)| i.intent_id.clone())
            .collect();
        tx.send_replace(Some(std::sync::Arc::new(on_registered)));

        let mut v = vec![intent("on-reg"), intent("on-inflight"), intent("on-reg-2")];
        assert!(gate.retain(&mut v));
        let ids: Vec<&str> = v.iter().map(|i| i.intent_id.as_str()).collect();
        assert_eq!(
            ids,
            vec!["on-reg", "on-reg-2"],
            "in-flight placements excluded from Job-create"
        );
    }

    /// §13d toleration axis (r31 bug_020): `apply_intent_resources`
    /// derives per-intent tolerations from `intent.hw_class_names ×
    /// HwClassConfig.taints_for(h)` — the SAME `[sla.hw_classes.$h]`
    /// map the scheduler used to compute `intent.node_affinity`.
    /// Without this, an intent affinity-pinned to a kvm-tainted metal
    /// node (via `node_affinity`) but spawned on a `features=[]` Pool
    /// (`wants_metal=false`) sat permanently Pending — the affinity
    /// passes only metal nodes but TaintToleration rejects them.
    // r[verify ctrl.pool.intent-tolerations]
    #[test]
    fn intent_hw_class_names_derive_taint_tolerations() {
        use rio_proto::types::{HwClassLabels, NodeTaint};
        let kvm_taint = || NodeTaint {
            key: "rio.build/kvm".into(),
            value: "true".into(),
            effect: "NoSchedule".into(),
        };
        let hw = HwClassConfig::default();
        hw.set(
            [
                (
                    "metal-x86".into(),
                    HwClassLabels {
                        taints: vec![kvm_taint()],
                        ..Default::default()
                    },
                ),
                ("mid-ebs-x86".into(), HwClassLabels::default()),
            ]
            .into(),
            (192, 1536 << 30),
        );

        // Pool whose static features DON'T include kvm — `wants_metal`
        // would not add the toleration. The intent is affinity-pinned
        // to metal because the scheduler routed `["nixos-test"]` there.
        let mut pool = test_pool("nt", ExecutorKind::Builder);
        pool.spec.features = vec!["nixos-test".into()];
        // Pass the test `hw` directly (was `HwClassConfig::default()`,
        // which masked the deleted pool-static nodeSelector). With the
        // real config, `wants_metal` is false (`metal-x86` carries no
        // `provides_features`) AND the precondition holds.
        let mut spec =
            pod::build_executor_pod_spec(&pool, &test_sched_addrs(), &test_store_addrs(), &hw);
        // Pre-condition (r34 bug_002): `wants_metal=false` in this
        // fixture (`metal-x86` carries no `provides_features`), so the
        // pool-static path adds neither nodeSelector nor toleration —
        // the per-intent toleration tested below is the only source.
        // The r33 bug_002 structural invariant (no pool-static kvm
        // nodeSelector under `wants_metal=true`) is the LOAD-BEARING
        // test at pod.rs::wants_metal_does_not_force_node_selector_on_
        // shared_feature.
        assert!(
            spec.node_selector
                .as_ref()
                .is_none_or(|ns| !ns.contains_key("rio.build/kvm")),
            "precondition: pool-static path adds no kvm nodeSelector"
        );
        // Pre-condition: no kvm toleration from the pool-static path.
        assert!(
            spec.tolerations
                .as_ref()
                .is_none_or(|ts| !ts.iter().any(|t| t.key.as_deref() == Some("rio.build/kvm"))),
            "precondition: pool-static path adds no kvm toleration"
        );

        let i = SpawnIntent {
            intent_id: "abc".into(),
            cores: 4,
            mem_bytes: 8 << 30,
            hw_class_names: vec!["metal-x86".into()],
            ..Default::default()
        };
        apply_intent_resources(&mut spec, &pool, &i, &hw);
        let tols = spec.tolerations.as_ref().expect("tolerations set");
        let kvm = tols
            .iter()
            .find(|t| t.key.as_deref() == Some("rio.build/kvm"))
            .expect("kvm toleration derived from intent.hw_class_names × taints_for");
        assert_eq!(kvm.value.as_deref(), Some("true"));
        assert_eq!(kvm.effect.as_deref(), Some("NoSchedule"));
        assert_eq!(kvm.operator.as_deref(), Some("Equal"));

        // Idempotent: re-applying must not duplicate the toleration
        // (or the pool-static one, if both sources fire — `wants_metal`
        // and the intent path).
        apply_intent_resources(&mut spec, &pool, &i, &hw);
        let n_kvm = spec
            .tolerations
            .as_ref()
            .unwrap()
            .iter()
            .filter(|t| t.key.as_deref() == Some("rio.build/kvm"))
            .count();
        assert_eq!(n_kvm, 1, "toleration deduped on re-apply");

        // Untainted hwClass → no extra toleration.
        let mut spec2 =
            pod::build_executor_pod_spec(&pool, &test_sched_addrs(), &test_store_addrs(), &hw);
        let before = spec2.tolerations.clone();
        let i2 = SpawnIntent {
            intent_id: "abc".into(),
            hw_class_names: vec!["mid-ebs-x86".into()],
            ..Default::default()
        };
        apply_intent_resources(&mut spec2, &pool, &i2, &hw);
        assert_eq!(
            spec2.tolerations, before,
            "untainted hwClass → no per-intent toleration"
        );
    }
}
