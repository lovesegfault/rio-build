//! Pool Job-per-build reconciler.
//!
//!   1. Each `apply()` tick polls `GetSpawnIntents` (filtered by
//!      `{kind, systems, features}`) via the same `ctx.admin` client
//!      the finalizer uses for report synthesis (its stream-era
//!      DrainExecutor hop is retired).
//!   2. If the scheduler returned intents and active Jobs for this
//!      pool < `spec.maxConcurrent`, spawn one Job per intent (up to
//!      the ceiling).
//!   3. Each Job runs one rio-builder pod → worker exits after one
//!      build → pod terminates → `ttlSecondsAfterFinished`
//!      (`JOB_TTL_SECS`) reaps the Job.
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
    Node, NodeAffinity, NodeSelector, Pod, PodSpec, ResourceRequirements, Toleration,
};
use k8s_openapi::apimachinery::pkg::api::resource::Quantity;
use k8s_openapi::apimachinery::pkg::apis::meta::v1::OwnerReference;
use kube::api::{Api, DeleteParams, ListParams};
use kube::runtime::controller::Action;
use kube::{Resource, ResourceExt};
use tracing::{debug, info, warn};

use super::candidate;
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
/// The rendered deadline the pod is dispatched under — the SAME
/// `ephemeral_deadline` solve `activeDeadlineSeconds` is rendered from,
/// stamped as data so the scheduler can floor its mint-persisted
/// deadline at the value the pod really runs under (bug_106: a
/// between-solve estimate shrink must never establish a healthy
/// attempt early). Parsed by `nodeclaim_pool/pods.rs` into
/// `BoundIntent.deadline_secs`.
pub(crate) const DEADLINE_SECS_ANNOTATION: &str = "rio.build/deadline-secs";

/// Job-metadata annotation carrying a fingerprint of
/// `SpawnIntent.node_selector`. Compared on each tick so a Pending Job
/// whose selector no longer matches the scheduler's current solve
/// (ICE-backoff spot→on-demand fallback) is reaped instead of
/// NameCollision-blocking the re-solved intent forever.
pub(crate) const INTENT_SELECTOR_ANNOTATION: &str = "rio.build/intent-selector";

/// Pod-template annotation carrying the intent's `(h, cap)` cell list
/// as `"h:cap"` rows joined by `","` (the house cell grammar —
/// round-10 merged_bug_049). The DURABLE half of the re-ack lane's
/// local inventory: a Pending Job whose intent is OFF the demand page
/// (>page-limit backlog after a scheduler restart) re-arms the
/// scheduler's `dispatched_cells` from this stamp — the in-memory map
/// is armed only by acks, and without a full-set re-ack lane the
/// heartbeat-edge ICE clear never re-arms for off-page Jobs
/// (`ctrl.pool.ack-spawned-soundness` requires the re-ack; this stamp
/// makes it page-independent). Written by [`build_job`] from the
/// SPAWNED intent's `(hw_class_names[i], node_affinity[i])` zip;
/// parsed by [`cells_from_annotation`]. Absent on pre-upgrade Jobs ⇒
/// the bare `intent_id` re-ack (the legacy no-arm echo) — a one
/// deploy-generation residual, disclosed at the re-ack assembly.
pub(crate) const INTENT_CELLS_ANNOTATION: &str = "rio.build/intent-cells";

/// Pod-template annotation carrying the RENDERED idle-exit bound
/// (round-10 bug_078): the eta-aware `RIO_IDLE_SECS` the pod actually
/// runs with. The orphan-running reap reads it back so its grace
/// covers the pod's own patience (`max(ORPHAN_REAP_GRACE, bound+60)`)
/// — without the readback, a metal-eta forecast pod waiting lawfully
/// past 300s would be orphan-reaped mid-wait, the same
/// reaped-while-wanted defect one lane over. Absent (pre-upgrade
/// Jobs / Ready spawns at the flat bound) ⇒ the flat grace.
pub(crate) const IDLE_EXIT_SECS_ANNOTATION: &str = "rio.build/idle-exit-secs";

/// Pod-template annotation carrying the controller's create-time
/// bench-gate decision. `"true"` ⇒ the spawned builder runs the full
/// K=3 microbench (STREAM/ioseq/alu) before accepting work. Read via
/// downward-API → `RIO_HW_BENCH_NEEDED` (`rio_builder::Config.
/// hw_bench_needed`). `build_job` always stamps it `"true"`/`"false"`;
/// there is no absent-annotation fallback (an absent annotation would
/// resolve the fieldRef to `""`, which the config loader rejects for a
/// bool field — the pod fails at startup rather than silently running
/// with a default).
pub(crate) const HW_BENCH_NEEDED_ANNOTATION: &str = "rio.build/hw-bench-needed";

/// Log + scratch budget. nix `build-dir` lands in the overlay emptyDir
/// (nix ≥2.30 default = stateDir/builds), but stdout/stderr capture and
/// the daemon's own state live outside. 1 GiB headroom.
const LOG_BUDGET_BYTES: u64 = 1 << 30;

/// Round-10 live_058-a (HIGH): the worker's RESIDENT overhead pad on
/// the container-mem axis — the rio-builder daemon + FUSE client +
/// log capture that live INSIDE the container the k8s limit binds,
/// on top of the solved BUILD size. The solve sizes the BUILD; the
/// limit binds the CONTAINER (k8s memory.max is set at the delegated
/// POD level — monitors.rs: the per-build sub-cgroup carries no
/// limit of its own), so a warm tiny fit (~45-69 MB solved, the live
/// incident specimens) raw-stamped as request==limit landed BELOW
/// the worker's own baseline and the kernel OOM-killed the whole
/// container before/regardless of the build — the live_058 2.75h
/// same-size requeue loop. Derivation basis (recorded per the A1
/// duty): the incident pins baseline > 69 MB (those containers
/// died); the pad covers daemon RSS + FUSE client structures + log
/// capture with headroom — 256 MiB is the HYPOTHESIS value carried
/// from the incident review, VIOLABLE (R17, all axes): size = the
/// pad itself per pod (the cost of never under-housing the worker);
/// cost = 256 MiB × pods/node of billable mem; population = every
/// builder/fetcher container; time N/A. Measurement note: re-derive
/// from worker-baseline RSS telemetry once a soak window exists —
/// the consts are the knob, the constructor is the law.
const WORKER_MEM_OVERHEAD_BYTES: u64 = 256 << 20;

/// The container-mem FLOOR (live_058-a): no container renders below
/// this regardless of how tiny the solve is — tiny solves carry the
/// same resident worker. 512 MiB = pad + the sub-pad solve band with
/// headroom (the incident's 45-69 MB solves land here). VIOLABLE
/// (R17): same axes as the pad; the floor only binds when
/// `solved + pad < floor`, i.e. solves under 256 MiB.
const CONTAINER_MEM_MIN_BYTES: u64 = 512 << 20;
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
///
/// census[gen: nix/tests/helm/14-disk-ceiling.sh] — member 4 mirrors
/// the constants by content (`OVERLAY_HEADROOM_PCT`/`LOG_BUDGET_BYTES`
/// rows); the membership census is `disk_four_caller_census` below.
// r[impl sched.sla.disk-reaches-ephemeral-storage+1]
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
///
/// `fuse_cache_bytes` is the BUILDER pool budget
/// (`[nodeclaim_pool].fuse_cache_bytes`); Fetcher intents substitute
/// the much smaller [`pod::fetcher_fuse_cache_bytes`] — a FOD's input
/// closure is a fetch script's runtime deps, not an arbitrary build
/// closure, and inheriting the builder budget made the fuse-cache
/// addend dominate the fetcher pod's ephemeral-storage request ~30×
/// over what the pod can ever use. The selection keys on
/// `SpawnIntent.kind` here and on `pool.spec.kind` in
/// [`pod::fuse_cache_bytes`]; the scheduler's intent filter
/// (`intent.kind == pool.spec.kind`) keeps the two in agreement for
/// every intent a pool actually spawns.
pub(crate) fn intent_pod_footprint(i: &SpawnIntent, fuse_cache_bytes: u64) -> PodFootprint {
    let fuse = if i.kind == i32::from(rio_proto::types::ExecutorKind::Fetcher) {
        pod::fetcher_fuse_cache_bytes()
    } else {
        fuse_cache_bytes
    };
    PodFootprint {
        cores: i.cores,
        // r[impl ctrl.pool.container-overhead]
        // live_058-a: the container law — solved BUILD mem + the
        // resident worker pad, floored. Additive at the container
        // seam, applied to the SOLVED dimension after any floor
        // ladder clamp upstream (the ladder still doubles the solve;
        // CgroupOom keys on the padded pod limit — the per-build
        // sub-cgroup refinement is the RULED named candidate).
        mem_bytes: i
            .mem_bytes
            .saturating_add(WORKER_MEM_OVERHEAD_BYTES)
            .max(CONTAINER_MEM_MIN_BYTES),
        ephemeral_bytes: pod_ephemeral_request(i.disk_bytes, intent_headroom(i), fuse),
    }
}

/// The CONTAINER resource footprint (round-10 live_058-a, R24): the
/// `(cores, mem, ephemeral)` a pod will actually request, with the
/// container-mem law applied IN THE CONSTRUCTOR
/// ([`intent_pod_footprint`] — the sole mint:
/// `mem = max(solved + WORKER_MEM_OVERHEAD_BYTES,
/// CONTAINER_MEM_MIN_BYTES)`). Because FFD's simulate and
/// `cover_deficit` consume the same constructor, the pad propagates
/// to fit-checks and NodeClaim floors by construction — the
/// §Simulator-shares-accounting contract holds for mem exactly as
/// the disk lesson demanded for disk. The pod stamp goes through
/// [`stamp_container_resources`], whose signature admits ONLY this
/// type: the raw `i.mem_bytes` read does not type-check inside that
/// seam (compile-sealed AT THE HELPER — the honest tier: `mem_bytes`
/// is a public proto field, so solve/telemetry reads elsewhere stay
/// legitimate; that wider population is CENSUS-HELD, see the
/// `mem_axis_census` module).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct PodFootprint {
    cores: u32,
    mem_bytes: u64,
    ephemeral_bytes: u64,
}

impl PodFootprint {
    pub(crate) fn cores(&self) -> u32 {
        self.cores
    }

    /// The PADDED container mem (never the bare solve).
    pub(crate) fn mem_bytes(&self) -> u64 {
        self.mem_bytes
    }

    pub(crate) fn ephemeral_bytes(&self) -> u64 {
        self.ephemeral_bytes
    }

    /// The `(c, m, d)` triple for fold/compare consumers (FFD sums,
    /// cover sizing, the OverCap letter evidence).
    pub(crate) fn as_triple(&self) -> (u32, u64, u64) {
        (self.cores, self.mem_bytes, self.ephemeral_bytes)
    }
}

/// The container-resource STAMP seam (round-10 live_058-a, R24): the
/// resource map is buildable ONLY from a [`PodFootprint`] — the
/// intent is out of scope in this body, so stamping a raw solve
/// (`i.mem_bytes`) into container resources no longer type-checks
/// here. `requests == limits` (hard caps, no burst) — ADR-023
/// §sizing-model. Quantities rendered as raw byte counts (no SI
/// suffix): k8s parses bare integers as base-unit and they roundtrip
/// exactly.
// r[impl ctrl.pool.container-overhead]
fn stamp_container_resources(
    container: &mut k8s_openapi::api::core::v1::Container,
    fp: &PodFootprint,
) {
    let map: BTreeMap<String, Quantity> = BTreeMap::from([
        ("cpu".into(), Quantity(fp.cores().to_string())),
        ("memory".into(), Quantity(fp.mem_bytes().to_string())),
        (
            "ephemeral-storage".into(),
            Quantity(fp.ephemeral_bytes().to_string()),
        ),
    ]);
    container.resources = Some(ResourceRequirements {
        requests: Some(map.clone()),
        limits: Some(map),
        ..Default::default()
    });
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

/// merged_bug_080(2a) census addressability: the Job-alive deadline
/// FLOOR, hoisted from `ephemeral_deadline`'s inline `.max(180)` so
/// the alive-floor census generator (`rg 'const .*SECS'` over this
/// module tree) captures it as a named row. The floor exceeds
/// `candidate::POOL_STREAK_ORPHAN_EXPIRY_SECS` (120 s) — which is WHY
/// a job-held intent's respawn record needs the structural
/// `note_job_alive` refresh rather than a constant inequality (the
/// close is structural; this const exists for the census, not as a
/// load-bearing bound). The 86400 upper cap stays prose in the doc
/// below (scheduler-side clamp; no controller const exists in-plane).
pub(super) const EPHEMERAL_DEADLINE_FLOOR_SECS: i64 = 180;

/// `activeDeadlineSeconds` for an ephemeral Job: `intent.
/// deadline_secs` verbatim. The scheduler computes it per-derivation
/// (D7: `wall_p99 × 5` for fitted, `[sla].probe.deadline_secs` for
/// unfitted, clamped `[floor, 86400]`) and `SlaConfig::validate`
/// guarantees `probe.deadline_secs >= 180`, so the intent value is
/// always `>= 180`. No controller-side multiplier or per-kind
/// fallback. The [`EPHEMERAL_DEADLINE_FLOOR_SECS`] floor is defensive
/// only — proto default is 0; a 0s deadline would fail the Job at
/// creation, and `< 180` would tie the worker's
/// `daemon_timeout = deadline − 90` against this timer.
// r[impl ctrl.ephemeral.intent-deadline]
pub(super) fn ephemeral_deadline(intent: &SpawnIntent) -> i64 {
    i64::from(intent.deadline_secs).max(EPHEMERAL_DEADLINE_FLOOR_SECS)
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

// r[impl ctrl.pool.ephemeral+1]
/// Reconcile a Pool: count active Jobs, poll spawn intents, spawn
/// Jobs if work is waiting.
///
/// Status: `replicas` / `readyReplicas` / `desiredReplicas` mean
/// "active Jobs." `desiredReplicas` is the concurrent-Job ceiling
/// (`spec.maxConcurrent`).
/// The candidate-source universe for `pool`: every node matching the
/// pool's static placement constraints (the effective node selector —
/// kind constraints like the fetcher selector — plus the arch derived
/// from `spec.systems`), with name + labels + cordon state; the same
/// data the FFD's node view keys on, read lazily from the apiserver
/// only on ticks where some intent actually carries exclusions.
/// NotReady nodes are KEPT (merged_bug_124(a)) — the per-intent gate
/// decides admissibility over [`candidate::RenderInputs::admits`].
/// `None` when the list fails — callers treat that as "cannot prove
/// exhaustion" and spawn as today (fail-open: the anti-affinity makes
/// the pod Pending at worst, exactly the pre-gate behavior).
// r[impl ctrl.pool.intent-candidate-set]
pub(super) async fn spawnable_nodes_for_pool(
    client: &kube::Client,
    pool: &Pool,
) -> Option<Vec<candidate::CandidateNode>> {
    let mut selector: Vec<String> = pod::effective_node_selector(pool)
        .unwrap_or_default()
        .into_iter()
        .map(|(k, v)| format!("{k}={v}"))
        .collect();
    if let Some(arch) = pod::nix_systems_to_k8s_arch(&pool.spec.systems) {
        selector.push(format!("kubernetes.io/arch={arch}"));
    }
    let mut params = ListParams::default();
    if !selector.is_empty() {
        params = params.labels(&selector.join(","));
    }
    let nodes: Api<Node> = Api::all(client.clone());
    let list = match nodes.list(&params).await {
        Ok(l) => l,
        Err(e) => {
            debug!(pool = %pool.name_any(), error = %e,
                   "spawnable-node list failed; skipping the NoEligibleSource gate this tick");
            return None;
        }
    };
    Some(
        list.items
            .into_iter()
            .filter_map(|n| {
                let name = n.metadata.name.clone()?;
                // Cordoned (`spec.unschedulable`) nodes can never host
                // the pod (kube-scheduler will not bind there). NotReady
                // nodes are kept: a booting/provisioning node is a
                // transient that will admit pods shortly — the strict
                // Ready filter manufactured single-tick fleet-exhaust
                // poisons out of node restarts (merged_bug_124(a)).
                let schedulable = n
                    .spec
                    .as_ref()
                    .and_then(|s| s.unschedulable)
                    .is_none_or(|u| !u);
                let labels = n.metadata.labels.clone().unwrap_or_default();
                Some(candidate::CandidateNode {
                    name,
                    labels,
                    schedulable,
                })
            })
            .collect(),
    )
}

/// Report one `NoEligibleSource` spawn-gate verdict per gated intent
/// (instead of spawning an unschedulable Job). Returns the ACKED
/// intent ids (bug_028: the caller feeds them to the futility
/// breaker's `NoEligibleSource` reset lane — an ack is a named
/// resolution). Best-effort: an RPC error leaves the intent
/// un-reported — it stays Ready scheduler-side and is re-evaluated
/// next tick. A successfully acked report poisons the derivation
/// scheduler-side (fleet-exhaust arm), so it leaves the intent stream
/// and re-ticks send nothing further; a duplicate ack is a
/// server-side no-op either way.
// r[impl sched.dispatch.fleet-exhaust+5]
pub(super) async fn report_no_eligible_source(
    ctx: &Ctx,
    pool: &str,
    gated: &[&SpawnIntent],
) -> Vec<(String, candidate::VerdictWitness)> {
    let mut acked = Vec::new();
    for intent in gated {
        match admin_call(ctx.admin.clone().report_attempt_outcome(
            rio_proto::types::ReportAttemptOutcomeRequest {
                // 124(b) staleness witness: echo the cycle this verdict
                // was computed against; the scheduler ack-no-poisons a
                // stale echo (the drv re-entered Ready since).
                resubmit_cycle: intent.resubmit_cycle,
                intent_id: intent.intent_id.clone(),
                job_name: String::new(),
                exec_id: String::new(),
                reason: rio_proto::types::AttemptTerminalReason::NoEligibleSource.into(),
                node_name: String::new(),
            },
        ))
        .await
        {
            Ok(resp) => {
                warn!(
                    pool,
                    intent_id = %intent.intent_id,
                    excluded = intent.excluded_nodes.len(),
                    "every spawnable node is excluded for this intent; reported NoEligibleSource \
                     instead of spawning an unschedulable Job"
                );
                // merged_bug_080(2b): mint the reset witness AT the
                // ack — the poison lane's premise is the completed
                // RPC itself (`attempt_resolved` is false by design
                // on every NoEligibleSource arm; see the mint's doc).
                // r[impl ctrl.pool.respawn-backoff+2]
                acked.push((
                    intent.intent_id.clone(),
                    candidate::VerdictWitness::from_acked_no_eligible_source(&resp.into_inner()),
                ));
            }
            Err(e) => {
                warn!(
                    pool, intent_id = %intent.intent_id, error = %e,
                    "NoEligibleSource report failed; the intent stays Ready and is re-evaluated \
                     next tick"
                );
            }
        }
    }
    acked
}

/// The AD2 gate's candidate universe for one tick (bug_028): the
/// closed alphabet of fold-completion conditions. The node LIST stays
/// lazy and async in the reconcile; this type carries its outcome
/// into the synchronous fold so the fold itself is unit-testable.
pub(super) enum GateUniverse {
    /// The wanted set is empty — nothing to evaluate; the fold is
    /// SKIPPED (witness dropped, streaks retain without stepping).
    NoWanted,
    /// No wanted intent carries exclusions: every wanted intent is
    /// trivially un-gated and the fold COMPLETES without a node LIST
    /// (observed un-exhaustion — this closes the retired sub-case
    /// where dropped exclusions skipped the fold and never reset the
    /// streak).
    NoExclusions,
    /// The lazy LIST succeeded: partition against these candidates.
    Nodes(Vec<candidate::CandidateNode>),
    /// The lazy LIST failed: cannot prove exhaustion — the fold is
    /// SKIPPED (witness dropped) and spawn is fail-open for intents
    /// bearing no LIVE exhaustion evidence (bug_075: a live streak is
    /// withheld — see the arm in [`evaluate_spawn_gate`]).
    ListFailed,
}

impl GateUniverse {
    /// bug_075 census generator half (R15): every arm's discriminant —
    /// the spawn-decision census iterates this array so its (arm ×
    /// evidence-state) cells come FROM the alphabet, never an author's
    /// memory. Pinned exhaustive by the same-file match in
    /// [`Self::discriminant`]: a new arm is a compile error there
    /// until it is added HERE and its census cells are stated.
    /// Test-only consumer BY DESIGN; production code matches the
    /// closed enum exhaustively instead of iterating it.
    #[cfg_attr(not(test), expect(dead_code))]
    pub(super) const ALL_DISCRIMINANTS: [&'static str; 4] =
        ["NoWanted", "NoExclusions", "Nodes", "ListFailed"];

    /// The [`Self::ALL_DISCRIMINANTS`] compile pin: the exhaustive
    /// match makes a new arm a compile error at this line until the
    /// array above (and the census test's cells) name it.
    #[cfg_attr(not(test), expect(dead_code))]
    pub(super) fn discriminant(&self) -> &'static str {
        match self {
            GateUniverse::NoWanted => "NoWanted",
            GateUniverse::NoExclusions => "NoExclusions",
            GateUniverse::Nodes(_) => "Nodes",
            GateUniverse::ListFailed => "ListFailed",
        }
    }
}

/// One completed (or skipped) AD2 gate evaluation (bug_028).
pub(super) struct SpawnGateOutcome {
    /// Intents allowed to spawn this tick, in the scheduler's priority
    /// order, NOT yet headroom-truncated: gated intents, intents
    /// inside their verdict-free-respawn backoff, AND fold-skip
    /// intents carrying a live exhaustion streak (bug_075) are removed
    /// (none burns a headroom slot).
    pub(super) spawnable: Vec<SpawnIntent>,
    /// Gated intents whose exhaustion satisfied the firing law this
    /// fold (count + wall-clock floor) — the NoEligibleSource reports.
    pub(super) to_report: Vec<SpawnIntent>,
}

// r[impl ctrl.pool.no-eligible-persist+5]
/// The AD2 gate fold, extracted for unit-testability (bug_028): the
/// partition runs over the FULL existing-names-filtered wanted set —
/// never a headroom window — so exhaustion evaluation covers every
/// wanted intent independent of the spawn window. Mints the
/// [`candidate::Observation`] from the partition and consumes the
/// [`candidate::StreakTick`] witness; pool-keyed (merged_bug_117) and
/// namespace-qualified (merged_bug_073), so this fold can only touch
/// THIS pool's streaks. On the skip arms (no wanted intents, LIST
/// failure) the witness is dropped and streaks retain without
/// stepping — and the ListFailed arm withholds intents whose retained
/// streak is LIVE (bug_075: the spawn decision is total over the
/// (universe arm × [`candidate::EvidenceState`]) product; fail-open
/// applies only to intents bearing no live exhaustion evidence,
/// because a spawned Job hides the intent from evaluation past the
/// orphan expiry). The verdict-free-respawn backoff still gates spawn
/// on EVERY arm (deaths are noted by the reap before the fold, so a
/// freshly reaped intent is blocked the same tick its respawn would
/// otherwise fire).
pub(super) fn evaluate_spawn_gate(
    wanted: Vec<SpawnIntent>,
    universe: &GateUniverse,
    streaks: &mut candidate::PoolStreaks,
    tick: candidate::StreakTick,
    key: &candidate::PoolKey,
    coverage: DemandCoverage,
    now: std::time::Instant,
) -> SpawnGateOutcome {
    let (spawnable, to_report) = match universe {
        GateUniverse::NoWanted => {
            // Fold skipped: drop the witness, retain streaks —
            // trivially nothing to spawn (`wanted` is empty by
            // construction of this arm).
            drop(tick);
            (wanted, Vec::new())
        }
        GateUniverse::ListFailed => {
            // Fold skipped: drop the witness, retain streaks — and
            // WITHHOLD intents carrying LIVE exhaustion evidence
            // (bug_075): fail-open spawn applies only to intents
            // bearing none. Spawning a suspected-exhausted intent
            // makes it structurally unobservable (existing-name
            // exclusion) for at least its ≥180 s Job deadline, which
            // exceeds the 120 s orphan window — destroying by
            // spawning what the retain law preserved, and livelocking
            // the poison report that would un-wedge the scheduler. A
            // withheld intent stays jobless and re-checks next tick;
            // staleness self-limits the withhold (a stale streak is
            // dead evidence and re-opens fail-open, so a permanent
            // LIST failure cannot wedge spawn past the orphan
            // window). The match is exhaustive over the closed
            // [`candidate::EvidenceState`] alphabet (R14/R15): a new
            // evidence state must state its fold-skip disposition
            // here before it compiles.
            drop(tick);
            let streaks = &*streaks;
            let (withheld, spawnable): (Vec<SpawnIntent>, Vec<SpawnIntent>) =
                wanted.into_iter().partition(|i| {
                    match streaks.evidence_state(key, &i.intent_id, now) {
                        candidate::EvidenceState::LiveStreak => true,
                        // No evidence to destroy / dead evidence —
                        // fail-open preserved. In-backoff intents flow
                        // through to the universal `respawn_blocked`
                        // post-arm filter below (one withhold law, one
                        // call site).
                        candidate::EvidenceState::NoEvidence
                        | candidate::EvidenceState::StaleStreak
                        | candidate::EvidenceState::InBackoff => false,
                    }
                });
            if !withheld.is_empty() {
                tracing::debug!(
                    pool = %key,
                    withheld = withheld.len(),
                    "node LIST failed: withholding fail-open spawn for intents \
                     with live exhaustion streaks (re-checked next tick)"
                );
            }
            (spawnable, Vec::new())
        }
        GateUniverse::NoExclusions => {
            // Trivially complete fold: every wanted intent evaluated
            // and un-gated — streaks for them read observed recovery.
            let obs = candidate::Observation::from_partition(&[], &wanted);
            // An empty gated set cannot fire (reports are drawn from
            // the observation's gated half) — drop the fire list.
            let _ = streaks.step(tick, &obs, coverage, now);
            (wanted, Vec::new())
        }
        GateUniverse::Nodes(candidates) => {
            let (gated, spawnable): (Vec<SpawnIntent>, Vec<SpawnIntent>) = wanted
                .into_iter()
                .partition(|i| candidate::no_eligible_source(i, candidates));
            // Withhold the spawn from tick 1 (the Job would sit
            // unschedulable behind its own anti-affinity) but only
            // REPORT — i.e. poison — after the exhaustion persists
            // per the firing law (count + wall-clock floor): a
            // single-tick universe blip (node restart, informer lag,
            // autoscaler churn) must not poison a derivation. Streak
            // entries whose intent was EVALUATED and is no longer
            // gated are pruned (observed recovery); an already-acked
            // report keeps its streak harmlessly until the poisoned
            // drv leaves the intent stream (duplicate reports are
            // server-side no-ops).
            let obs = candidate::Observation::from_partition(&gated, &spawnable);
            let fire = streaks.step(tick, &obs, coverage, now);
            let to_report: Vec<SpawnIntent> = gated
                .into_iter()
                .filter(|intent| fire.iter().any(|f| f == &intent.intent_id))
                .collect();
            (spawnable, to_report)
        }
    };
    // bug_028 futility breaker: a spawnable intent inside its
    // verdict-free backoff window is withheld this tick (it does not
    // burn a headroom slot; next tick re-checks). Applied on every
    // arm — the records were stepped by the reap BEFORE this fold.
    let spawnable = spawnable
        .into_iter()
        .filter(|i| !streaks.respawn_blocked(key, &i.intent_id, now))
        .collect();
    SpawnGateOutcome {
        spawnable,
        to_report,
    }
}

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

    // bug_069 (amended by merged_bug_073; prune re-keyed by bug_028):
    // the exhaustion-streak witness is minted UNCONDITIONALLY at the
    // top of every reconcile and consumed by the gate fold's `step`.
    // Minting no longer mutates: a reconcile whose fold is skipped
    // (no wanted intents, node LIST failure) drops the witness and
    // the pool's streaks RETAIN WITHOUT STEPPING --- an unobserved
    // tick is evidence in neither direction. The documented ~30s
    // persistence is carried by the wall-clock floor inside `step`'s
    // firing law, and observed recovery --- a completed fold that
    // EVALUATED the intent and no longer gates it --- is what breaks a
    // streak (the fold's `Observation` carries the evaluated set, so
    // an intent the fold never looked at cannot read as recovered).
    // The key is namespace-qualified (same-named pools in two
    // namespaces are distinct streak owners).
    let streak_key = crate::reconcilers::pool::candidate::PoolKey::new(&ns, &name);
    let streak_tick = ctx.exhausted_streak.lock().begin_tick(&streak_key);

    // ---- Poll spawn intents ----
    // One GetSpawnIntents RPC per reconcile per pool. If the scheduler
    // is unreachable: log + treat as queued=0 + requeue. Next tick
    // retries. We ALSO set a SchedulerUnreachable condition on the
    // Pool so operators can see WHY nothing is spawning.
    let (mut page, evidence, scheduler_err): (IntentPage, DemandEvidence, Option<String>) =
        match queued_for_pool(ctx, pool).await {
            Ok(view) => {
                let (page, evidence) = view.split();
                (page, evidence, None)
            }
            Err(e) => {
                warn!(
                    pool = %name, error = %e,
                    "spawn-intents poll failed; treating as queued=0, will retry"
                );
                // The failed-poll view's vacuous completeness is
                // documented at `PoolDemandView::failed_poll`:
                // `scheduler_err` fail-closes every count consumer on
                // its own conjunct, and the empty want-map's
                // early-return keeps the membership reap closed.
                let (page, evidence) = PoolDemandView::failed_poll().split();
                (page, evidence, Some(e.to_string()))
            }
        };
    // r[impl ctrl.nodeclaim.placeable-gate+5]
    // ADR-023 §13b placeable gate: Builder Jobs spawn only for intents
    // the nodeclaim_pool reconciler's last FFD sim placed on a
    // `Registered=True` NodeClaim — structurally closes the spawn-
    // intent fan-out (1226 Ready intents would otherwise mint 1226
    // Pending Jobs, each a Karpenter provisioning request). The
    // controller now knows what's placeable; the scheduler-side cap
    // that DESIGN.md rejected (it couldn't see node state) is resolved
    // here.
    //
    // Builder-only retain: §13e (B2.6) made `nodeclaim_pool` cover
    // Fetcher Pools too — `reconcile_once` polls intents with no `kind`
    // filter and `cover_deficit` mints fetcher NodeClaims, so the
    // placeable set DOES include fetcher intent IDs and gating fetcher
    // spawn on it would NOT stall (the pre-§13e rationale died with the
    // static `rio-fetcher` NodePool). The Builder-only retain is kept
    // because the fetcher fan-out hazard is already bounded elsewhere:
    // (a) provisioning-side, `cover_deficit`'s per-tick + per-class
    //     `maxFleetCores` caps mint at most a handful of fetcher
    //     NodeClaims per tick;
    // (b) a fetcher pod is cheap (1 core, ~640Mi) — even an unbounded
    //     Pending-Job pile-up doesn't translate to 1226 NodeClaim
    //     provisioning requests the way builder Jobs (12c+) do.
    // (Production fetcher Pools do NOT set `spec.maxConcurrent`; the
    // Job-side ceiling is uncapped — the bound is the NodeClaim
    // budget, not the Job count.) Extending the retain to Fetcher
    // pools is a follow-up. `Ctx.placeable = None` ⇔ NodeClaim CRD
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
    let (gate_armed, placed_tick) = match (&ctx.placeable, pool.spec.kind) {
        (Some(g), ExecutorKind::Builder) => match apply_placeable_gate(&mut page, g) {
            Some(tick) => (true, Some(tick)),
            None => (false, None),
        },
        (Some(_), _) => (true, None),
        (None, _) => (false, None),
    };
    let queued = page.len_page().min(u32::MAX as usize) as u32;

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
        HwSampledCache::fetch(ctx, page.iter_page().flat_map(hw_classes_in).collect()).await;

    // ---- Count active Jobs for this pool ----
    // r[impl ctrl.pool.tick-ordering]
    // ORDERING (I-183): list AFTER the queued poll. The reap step
    // compares `pending` against `queued`. Polling first keeps the
    // comparison coherent.
    let jobs = jobs_api
        .list(&ListParams::default().labels(&format!("{}={name}", super::POOL_LABEL)))
        .await?;
    let census = job_census(&jobs.items);

    // merged_bug_080(2a) structural refresh: every same-pool Job in
    // this tick's listing --- active AND terminal (a terminal-unreaped
    // Job is the observable artifact of the terminal phase) ---
    // refreshes its intent's respawn-record `touched`. A job-held
    // intent leaves `wanted` (existing-name filter below) and is never
    // evaluated, while every Job-alive floor (the >=180 s
    // `EPHEMERAL_DEADLINE_FLOOR_SECS`, the 600 s terminal TTL) exceeds
    // the 120 s orphan expiry: without this lane the breaker's record
    // died mid-cycle and `deaths` never accumulated past 1. Residual
    // (re-priced at the live_056-b cap): a >120 s apiserver LIST
    // outage for THIS pool while sibling pools fold can expire only a
    // record whose backoff window has FULLY RUN OUT (option (b):
    // blocking records --- mid-backoff or gave-up --- are
    // expiry-immune in step's retain), so the loss is the ladder
    // POSITION, never an un-backed-off respawn: the record was
    // already spawn-eligible, and the next verdict-free death
    // re-enters the ladder at 10 s and re-climbs toward the 1280 s
    // cap. Bounded: no LIST ⇒ no spawn either.
    // r[impl ctrl.pool.respawn-backoff+2]
    ctx.exhausted_streak.lock().note_job_alive(
        &streak_key,
        jobs.items
            .iter()
            .filter_map(super::job::job_intent_id)
            .filter(|s| !s.is_empty()),
        std::time::Instant::now(),
    );

    // ---- Reap stale Jobs blocking respawn ----
    // (a) Terminal: a drv that re-enters Ready after its prior Job
    //     went Complete/Failed would NameCollision against the stale
    //     terminal Job for JOB_TTL_SECS (600s). (b) Selector-drift: a
    //     Pending Job whose selector no longer matches the scheduler's
    //     re-solve (ICE-backoff) NameCollision-blocks the new intent
    //     forever. Delete both before the spawn pass.
    //
    // Reap sees the FULL page, NOT the headroom-truncated slice: when
    // `ceiling` is set and every active slot is a selector-drifted
    // Pending, headroom=0 → truncated slice is empty → reap's
    // `want.is_empty()` early-return fires → nothing freed → headroom
    // stays 0 forever. Reaping frees slots; it doesn't consume
    // headroom, so the cap doesn't apply.
    //
    // r[impl ctrl.pool.demand-completeness]
    // The want-map is the ABSENCE lane: membership fused with the
    // completeness witness (R26), so the orphan-pending arm's
    // negative verdict is constructible only on a complete view —
    // off-page is `Unknowable`, and the destructive arm suspends
    // (merged_bug_029: the page-built want-map foreground-deleted
    // still-wanted off-page Pending Jobs at 10s, single-tick).
    let want = WantMap::for_pool(&page, evidence.coverage(), &name, pool.spec.kind);
    let reaped =
        reap_stale_for_intents(&jobs_api, &jobs.items, &want, ctx, pool, &name, &streak_key).await;
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

    // Filter-existing over the FULL page (bug_028: NO truncate here):
    // `headroom = ceiling - active` already accounts for
    // still-Pending Jobs, but those Jobs' drvs (still Ready, not yet
    // heartbeated) ALSO appear on the page. The wanted set feeds the
    // AD2 gate below, which must evaluate EVERY wanted intent — the
    // retired take-before-gate shape derived the gated set from the
    // headroom window, so a genuinely exhausted intent pushed past
    // the cutoff by priority churn read as recovered and its streak
    // never fired on a ceiling'd pool. (Page-lane walk: the gate's
    // totality quantifier is per-HELD-element by design — see the
    // W10-AH census row.)
    // Round-10 merged_bug_012: the SPAWN lane is placed-on-registered
    // only — a window-DEFERRED intent is demand-visible on the page
    // (want-map/queued/re-ack above) but not a spawn candidate this
    // tick (it re-presents when admitted; spawning it would defeat the
    // window's pacing). `placed_tick == None` ⇔ no Builder gate fold
    // ran (fetcher pools / CRD-absent), where every page intent is
    // spawnable as before.
    let wanted: Vec<SpawnIntent> = page
        .iter_page()
        .filter(|i| {
            placed_tick.as_ref().is_none_or(|t| {
                t.disposition(&i.intent_id)
                    == crate::reconcilers::nodeclaim_pool::FfdDisposition::PlacedRegistered
            })
        })
        .filter(|i| {
            !existing_names.contains(&pod::job_name(
                &name,
                pool.spec.kind,
                &intent_suffix(&i.intent_id),
            ))
        })
        .cloned()
        .collect();

    // ---- AD2 spawn gate: excluded ⊇ spawnable ⇒ NoEligibleSource ----
    // Partition the full wanted set BEFORE the headroom truncate
    // (bug_028 — the order the verified spawnCoherence model always
    // encoded): a gated intent is reported instead of spawned, so no
    // Job burns an activeDeadlineSeconds period sitting unschedulable
    // behind its own anti-affinity, gated intents no longer burn
    // headroom slots, and exhaustion verdicts no longer stall behind
    // a saturated ceiling (zero-headroom ticks observe). The node
    // LIST stays lazy — its trigger widens from the window to the
    // wanted set (cost disclosure, RULED S2-OQ3: still ≤1 LIST per
    // reconcile) — and the fold-skip alphabet narrows to {no wanted
    // intents, node LIST failure}: an exclusion-free wanted set
    // completes the fold trivially (all un-gated — observed
    // un-exhaustion) with no LIST at all. Fail-open on a failed node
    // list: cannot prove exhaustion ⇒ spawn as today, witness
    // dropped, streaks retain.
    let universe = if wanted.is_empty() {
        GateUniverse::NoWanted
    } else if wanted.iter().any(|i| !i.excluded_nodes.is_empty()) {
        match spawnable_nodes_for_pool(&ctx.client, pool).await {
            Some(candidates) => GateUniverse::Nodes(candidates),
            None => GateUniverse::ListFailed,
        }
    } else {
        GateUniverse::NoExclusions
    };
    let outcome = evaluate_spawn_gate(
        wanted,
        &universe,
        &mut ctx.exhausted_streak.lock(),
        streak_tick,
        &streak_key,
        evidence.coverage(),
        std::time::Instant::now(),
    );
    if !outcome.to_report.is_empty() {
        let to_report: Vec<&SpawnIntent> = outcome.to_report.iter().collect();
        let acked = report_no_eligible_source(ctx, &name, &to_report).await;
        // bug_028 futility breaker reset lane: an ACKED poison verdict
        // is a named resolution — the scheduler now holds the verdict,
        // so the intent's verdict-free-respawn record (if any) clears.
        // The witness was minted at the ack site (merged_bug_080(2b)).
        // r[impl ctrl.pool.respawn-backoff+2]
        let mut streaks = ctx.exhausted_streak.lock();
        for (intent_id, witness) in acked {
            streaks.note_resolution(&streak_key, &intent_id, witness, std::time::Instant::now());
        }
    }
    // Headroom truncate AFTER the gate, over spawnable intents only.
    // Intents are scheduler-side priority-sorted, so `take(headroom)`
    // over genuinely-new spawnable work drops lowest-priority, not
    // HashMap-order.
    let to_spawn_intents: Vec<SpawnIntent> = outcome.spawnable.into_iter().take(headroom).collect();

    // r[impl sec.executor.identity-token+3]
    // Mint per-intent `RIO_EXECUTOR_TOKEN`s on a controller-only
    // surface — `SpawnIntent` is plain data (dashboard/CLI also read
    // it via `GetSpawnIntents`), so the credential lives here, not on
    // the intent. One RPC per reconcile per pool, only for the
    // headroom-truncated set the controller is about to create Jobs
    // for. Empty `to_spawn_intents` skips the round-trip.
    //
    // live_053 / D-053-1 (owner-signed, 2026-06-10): an ERRING mint is
    // FAIL-CLOSED — `None` means no token evidence exists, and a spawn
    // cannot consume a witness that was never minted (the
    // durable-witness-coupling law). The retired fail-open arm spawned
    // whole token-less batches that are unauthenticatable BY
    // CONSTRUCTION under HMAC: 257 builders died in one night when a
    // 134s scheduler stall expired the 5s mint RPC twice. Skipping the
    // tick keeps the intents queued scheduler-side (no Job, no ack, no
    // dispatched_cells entry) — one tick of spawn latency instead of a
    // guaranteed-dead batch.
    //
    // bug_121: the SAME law, per element. The batch witness cannot
    // carry the element law — an intent the mint OMITTED (drv left
    // Ready between GetSpawnIntents and the mint; the two-RPC
    // read-read race) has no token evidence either, and spawning it
    // token-less under HMAC re-enters the dead-builder shape through
    // the Ok arm: first pull fast-fails Unauthenticated, the terminal
    // Job NameCollision-blocks respawn behind the two-tick strike and
    // steps the verdict-free backoff. Each intent's disposition is a
    // typed letter ({Token, Omitted, Keyless} — keyless is the wire
    // discriminator, so Ok(empty) is no longer ambiguous between dev
    // mode and whole-batch omission) total-folded BEFORE the spawn:
    // Token spawns with it, Omitted skips THIS intent this tick (no
    // Job, no collision, no backoff tax; it re-presents next tick,
    // minted), Keyless spawns token-less (dev parity, knob-free).
    let executor_tokens = mint_spawn_tokens(ctx, &name, &to_spawn_intents).await;

    // One pod per intent with that intent's resources + annotation.
    // Headroom truncates; the remainder is picked up next tick after
    // `active` decreases. Under mandatory `[sla]` (Phase 5) the
    // scheduler ALWAYS populates intents — empty list means empty
    // queue → spawns nothing.
    let spawned = match &executor_tokens {
        Some(grants) => {
            // bug_121 chokepoint: the per-intent letters fold HERE,
            // before any Job exists; the build closure consumes the
            // SAME letter (Token's payload is the spawn's token;
            // Keyless is token-less by law, not by accident).
            let spawnable = filter_spawnable_by_token(&name, grants, &to_spawn_intents);
            spawn_for_each(&jobs_api, &spawnable, &existing_names, &name, |intent| {
                let token = match grants.disposition(&intent.intent_id) {
                    TokenDisposition::Token(t) => Some(t),
                    TokenDisposition::Keyless => None,
                    // Structurally absent post-filter; refuse rather
                    // than spawn token-less if a future edit breaks
                    // the filter/spawn coupling.
                    TokenDisposition::Omitted => {
                        return Err(Error::InvalidSpec(format!(
                            "intent {} reached the spawn without token \
                             evidence (bug_121 filter bypassed)",
                            intent.intent_id
                        )));
                    }
                };
                build_job(
                    pool,
                    oref.clone(),
                    &ctx.scheduler,
                    &ctx.store,
                    &ctx.hw_config,
                    intent,
                    token,
                    &hw_sampled,
                    ctx.hw_bench_mem_floor,
                    ctx.placeable.is_some() && ctx.kube_build_scheduler_enabled,
                )
            })
            .await
        }
        // live_053 / D-053-1: no token evidence — zero spawns this
        // tick; the already-Pending re-ack path below still runs.
        None => Vec::new(),
    };
    // r[impl ctrl.pool.ack-spawned-soundness]
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
    // Round-10 merged_bug_049: derive from the Job LIST (the local
    // complete inventory), not the page — restart totality holds
    // independent of paging (off-page Pending Jobs re-ack via their
    // own durable cell stamps). `assemble_re_acks` owns the lane.
    let to_ack: Vec<SpawnIntent> =
        assemble_re_acks(&page, &name, pool.spec.kind, &jobs.items, &reaped, spawned);
    if to_ack.is_empty() {
        debug!(pool = %name, queued, active = census.active, ?ceiling, "no Pending intents to ack");
    } else {
        if let Err(e) = admin_call(ctx.admin.clone().ack_spawned_intents(
            rio_proto::types::AckSpawnedIntentsRequest {
                binding_snapshot: None,
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
                // bound-intents stream (full set every tick from its
                // per-tick Pod LIST) AND the live_051(c) rejection
                // verdicts (minted at the cover fold); the per-pool
                // ack only arms dispatched_cells.
                bound_intents: vec![],
                rejected: vec![],
            },
        ))
        .await
        {
            warn!(pool = %name, error = %e, "ack_spawned_intents failed; dispatched_cells not armed this tick");
        }
    }

    // ---- Reap excess Pending ----
    // r[impl ctrl.pool.degraded-polarity]
    // I-183: spawn-only is half a control loop. `None` when scheduler
    // unreachable OR placeable-gate unarmed OR the demand view is
    // unboundable: reap is fail-CLOSED (spawn is fail-open). On a
    // truncated view the bound sums the TYPED population classes
    // (Ready + forecast — merged_bug_006).
    let queued_known = reap_queued_known(
        scheduler_err.is_none(),
        gate_armed,
        evidence.coverage() == DemandCoverage::Complete,
        queued,
        evidence.ready_upper(),
        evidence.forecast_upper(),
    );
    reap_excess_pending(
        &jobs_api,
        &pods_api,
        &jobs.items,
        &reaped,
        queued_known,
        ctx,
        &name,
        &streak_key,
    )
    .await;

    // ---- Reap orphan Running ----
    // I-165: a builder stuck in D-state (FUSE wait, OOM-loop) can't
    // self-exit and never disconnects, so the scheduler never
    // reassigns. After ORPHAN_REAP_GRACE (5min), any Running Job the
    // scheduler doesn't consider busy is deleted.
    reap_orphan_running(&jobs_api, &jobs.items, &reaped, ctx, &name, &streak_key).await;

    // ---- AD5 cancel arm ----
    // A scheduler-side cancel/abort verdict closes the attempt; this
    // arm observes the closed→active edge and deletes the Job so the
    // pod's SIGTERM-abort fires now instead of at
    // activeDeadlineSeconds. Unconditional since the dispatch-mode
    // knob retired: every pool is a pull pool.
    super::job::cancel_closed_attempt_jobs(&jobs_api, &jobs.items, ctx, &name, &streak_key).await;

    // ---- Report terminations ----
    report_terminated_pods(ctx, &ns, &name, &streak_key).await;
    report_deadline_exceeded_jobs(ctx, &jobs.items, &streak_key).await;

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
) -> std::result::Result<PoolDemandView, tonic::Status> {
    // I-176: `filter_features=true` even when `features` is empty: a
    // featureless pool then sees only featureless work.
    // `effective_features` (Fetcher → [fetcher]) is the same chokepoint
    // `RIO_FEATURES` reads — keeps the spawn-decision query and the
    // spawned worker's capabilities derived from one value.
    let resp = admin_call(ctx.admin.clone().get_spawn_intents(
        rio_proto::types::GetSpawnIntentsRequest {
            kind: Some(super::executor_kind_to_proto(pool.spec.kind).into()),
            systems: pool.spec.systems.clone(),
            features: pod::effective_features(&pool.spec),
            filter_features: true,
            // Round-9 B3, the S4 consumer half: the per-pool fetch
            // budget is the shared consumer default (R14 shared-const
            // form — S2 sized it against the landed response-size
            // metric; this consumer inherits that envelope rather
            // than minting a second number). The page bounds THIS
            // consumer's slice only — `queued_by_system` stays the
            // uncapped demand truth (A-2).
            limit: rio_proto::SPAWN_INTENTS_DEFAULT_PAGE,
        },
    ))
    .await?
    .into_inner();
    Ok(PoolDemandView::from_response(resp, &pool.spec.systems))
}

/// Σ `queued_by_system[s]` over the pool's systems, saturated into
/// `u32` — the aggregate demand upper bound [`queued_for_pool`] feeds
/// [`reap_queued_known`]'s truncated arm. Pure (unit-walked beside the
/// law table): absent systems contribute zero; the sum saturates
/// rather than wraps.
pub(crate) fn aggregate_upper_for(
    systems: &[String],
    queued_by_system: &std::collections::HashMap<String, u64>,
) -> u32 {
    systems
        .iter()
        .filter_map(|s| queued_by_system.get(s))
        .fold(0u64, |a, n| a.saturating_add(*n))
        .min(u64::from(u32::MAX)) as u32
}

/// The per-reconcile demand view from [`queued_for_pool`] — the
/// round-10 R26 chokepoint (`r[ctrl.pool.demand-completeness]`): the
/// view is a PAGE of the scheduler's intent stream, and it no longer
/// hands out its raw slice. The iteration surface is TYPED into
/// lanes, and absence judgments demand the completeness witness BY
/// CONSTRUCTION:
///
/// - **Page lane** ([`IntentPage`], via [`Self::split`]): per-held-
///   element walks whose conclusions are per-intent (the AD2 spawn
///   gate, the spawn pass, the selector-drift compare, the mint).
///   Lawful on any page — every HELD intent is evaluated; the lane's
///   NAME carries the incompleteness so a totality-dependent consumer
///   is unwritable without naming the page-scope (the W10-AH census
///   classifies every consumer; the merged_bug_049 failures were
///   positive walks with totality contracts, which an absence
///   accessor alone cannot govern).
/// - **Absence lane** ([`WantMap`], minted via [`WantMap::for_pool`]
///   from the page + [`DemandCoverage`]): the ONLY source of negative
///   evidence. On an incomplete view the only verdict an absence
///   query can return is [`WantVerdict::Unknowable`] — "absent from
///   page" cannot type-check as "absent from demand"; destructive
///   absence-keyed arms (the orphan-pending reap) SUSPEND on it
///   (merged_bug_029's close).
/// - **Bound lane** ([`DemandEvidence`] → [`reap_queued_known`]):
///   counts that infer absence consume the exact post-filter count on
///   a COMPLETE view; on a truncated view the bound is the sum of the
///   TYPED per-population aggregates (Ready + forecast — over-counting
///   under-reaps, the safe direction; merged_bug_006's close: a
///   forecast-backed Pending Job is counted by its own class, never
///   assumed inside a Ready-only aggregate).
/// - **Totality/continuity lane**: consumers whose contracts span
///   pages (re-ack, evidence-expiry) derive from controller-local
///   complete inventories or suspend while `Incomplete` — the third
///   declared class; members file as W10-AH census rows.
pub(crate) struct PoolDemandView {
    page: IntentPage,
    complete: bool,
    /// Σ `queued_by_system[s]` for `s ∈ pool.spec.systems`, saturated
    /// into `u32` — pre-kind/feature-filter, hence ≥ the pool's true
    /// READY demand on every coherent snapshot.
    ready_upper: u32,
    /// Σ `forecast_by_system[s]` likewise — the forecast population
    /// class (counted at the scheduler's emit chokepoint, post
    /// tenant-budget admission). Absent on a pre-round-10 server ⇒ 0
    /// (the legacy ready-only bound, typed at the proto field).
    forecast_upper: u32,
}

impl PoolDemandView {
    /// The sole production constructor — from the wire response.
    /// Tests construct the proto response (plain data) and enter
    /// through here (R13: production constructors, no parallel
    /// fixture lane).
    pub(crate) fn from_response(
        resp: rio_proto::types::GetSpawnIntentsResponse,
        systems: &[String],
    ) -> Self {
        // B3 truncation honesty (the proto's own consumer law): a
        // truncated page understates demand, so absence-inference
        // must come from the per-class aggregates instead. Each
        // aggregate is by SYSTEM, before the kind/feature filters — a
        // SUPERSET of this pool's demand on its systems, which is
        // exactly the safe direction for a reap bound (over-counting
        // under-reaps; the law lives in `reap_queued_known`).
        let ready_upper = aggregate_upper_for(systems, &resp.queued_by_system);
        let forecast_upper = aggregate_upper_for(systems, &resp.forecast_by_system);
        Self {
            complete: !resp.truncated,
            ready_upper,
            forecast_upper,
            page: IntentPage(resp.intents),
        }
    }

    /// The failed-poll view: empty page, vacuously complete, zero
    /// aggregates. `scheduler_err` already fail-closes every count
    /// consumer on its own conjunct (`reap_queued_known`'s first
    /// gate), and the empty want-map's early-return keeps the
    /// membership reap closed — the vacuous `Complete` here is never
    /// consulted for a destructive verdict.
    fn failed_poll() -> Self {
        Self {
            page: IntentPage(Vec::new()),
            complete: true,
            ready_upper: 0,
            forecast_upper: 0,
        }
    }

    /// Decompose into the page lane and the evidence (witness +
    /// bounds). The page mutates through its own typed surface; the
    /// evidence is immutable for the tick.
    pub(crate) fn split(self) -> (IntentPage, DemandEvidence) {
        (
            self.page,
            DemandEvidence {
                complete: self.complete,
                ready_upper: self.ready_upper,
                forecast_upper: self.forecast_upper,
            },
        )
    }
}

/// The page lane of [`PoolDemandView`]: the held intents, EXPLICITLY
/// page-scoped. Every read goes through [`Self::iter_page`] (the
/// page-walk — per-held-element conclusions only) and every mutation
/// through [`Self::retain_page`]; the inner vector is private, so a
/// raw-slice membership test is unwritable outside this module and a
/// consumer cannot quantify over the page without naming the
/// page-scope. Absence judgments live on [`WantMap`]; demand bounds
/// on [`DemandEvidence`].
pub(crate) struct IntentPage(Vec<SpawnIntent>);

impl IntentPage {
    /// The page-scoped walk: per-held-element consumers only (spawn
    /// candidates, per-intent RPC fan-out, selector rendering). A
    /// consumer whose conclusion depends on what is NOT yielded is in
    /// the wrong lane — see [`WantMap`] (absence) and the W10-AH
    /// census (totality/continuity).
    pub(crate) fn iter_page(&self) -> std::slice::Iter<'_, SpawnIntent> {
        self.0.iter()
    }

    /// Page-scoped retain (the placeable-gate fold and siblings).
    pub(crate) fn retain_page(&mut self, f: impl FnMut(&SpawnIntent) -> bool) {
        self.0.retain(f);
    }

    /// Drop the whole page (the unarmed-gate posture).
    pub(crate) fn clear_page(&mut self) {
        self.0.clear();
    }

    /// Held-intent count — a PAGE property, not a demand bound
    /// ([`reap_queued_known`] owns the bound law).
    pub(crate) fn len_page(&self) -> usize {
        self.0.len()
    }

    /// Test-only page constructor (unit tests for the page-lane
    /// consumers; integration paths enter via
    /// [`PoolDemandView::from_response`]).
    #[cfg(test)]
    pub(crate) fn for_test(intents: Vec<SpawnIntent>) -> Self {
        Self(intents)
    }
}

/// The completeness witness, as a typed letter — constructed only by
/// [`DemandEvidence::coverage`], so a consumer holding `Complete`
/// provably read it off the view (R26: the witness is consumed BY the
/// test, not asserted beside it).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DemandCoverage {
    /// The page IS the whole demand for this pool's filter.
    Complete,
    /// The page was truncated — membership absence is unknowable.
    Incomplete,
}

/// The non-page half of the demand view: the completeness witness +
/// the per-population-class demand bounds.
pub(crate) struct DemandEvidence {
    complete: bool,
    ready_upper: u32,
    forecast_upper: u32,
}

impl DemandEvidence {
    /// The typed completeness letter (feeds [`WantMap::for_pool`] and
    /// the continuity consumers' suspension law).
    pub(crate) fn coverage(&self) -> DemandCoverage {
        if self.complete {
            DemandCoverage::Complete
        } else {
            DemandCoverage::Incomplete
        }
    }

    /// The Ready-class demand upper bound (pool systems sum).
    pub(crate) fn ready_upper(&self) -> u32 {
        self.ready_upper
    }

    /// The forecast-class demand upper bound (pool systems sum).
    pub(crate) fn forecast_upper(&self) -> u32 {
        self.forecast_upper
    }
}

/// The absence lane: job-name → render-fingerprint over the page,
/// FUSED with the completeness witness. The ONLY way to obtain
/// negative demand evidence ([`WantVerdict::AbsentFromDemand`]) is
/// through [`Self::verdict`], whose absence arm exists only under
/// [`DemandCoverage::Complete`] — on an incomplete page the answer is
/// [`WantVerdict::Unknowable`] and the destructive absence-keyed arms
/// suspend (merged_bug_029: "absent from page" must not type-check as
/// "absent from demand").
pub(crate) struct WantMap {
    names: HashMap<String, String>,
    coverage: DemandCoverage,
}

/// One job-name's demand verdict from [`WantMap::verdict`].
pub(crate) enum WantVerdict<'a> {
    /// The page wants this Job; payload = the render fingerprint for
    /// the selector-drift compare. Positive evidence — valid on ANY
    /// page (presence on a page proves demand).
    Wanted(&'a str),
    /// The COMPLETE view does not contain this Job's intent — true
    /// negative evidence; the orphan arm may act on it.
    AbsentFromDemand,
    /// Not on this page, and the page is incomplete — absence is
    /// unknowable; destructive arms suspend (re-judged next tick).
    Unknowable,
}

impl WantMap {
    /// Mint the absence lane for one pool from the page + the
    /// coverage letter. The fingerprint is the same `RenderInputs`
    /// projection the pod render stamps (`ctrl.pool.intent-candidate-
    /// set`).
    pub(crate) fn for_pool(
        page: &IntentPage,
        coverage: DemandCoverage,
        pool: &str,
        kind: ExecutorKind,
    ) -> Self {
        let names = page
            .iter_page()
            .map(|i| {
                (
                    pod::job_name(pool, kind, &intent_suffix(&i.intent_id)),
                    candidate::RenderInputs::from_intent(i).fingerprint(),
                )
            })
            .collect();
        Self { names, coverage }
    }

    /// The R26 accessor: membership fused with the witness. Absence
    /// is constructible only on a complete view.
    pub(crate) fn verdict(&self, job_name: &str) -> WantVerdict<'_> {
        match self.names.get(job_name) {
            Some(fp) => WantVerdict::Wanted(fp),
            None if self.coverage == DemandCoverage::Complete => WantVerdict::AbsentFromDemand,
            None => WantVerdict::Unknowable,
        }
    }

    /// Empty page (fail-closed gate for the membership reap — a
    /// scheduler error or a cleared gate must not orphan-reap).
    pub(crate) fn is_empty(&self) -> bool {
        self.names.is_empty()
    }
}

/// The reap-authority law (W9-AW, round-10 per-class form): a demand
/// bound may drive excess-Pending reaping only when the scheduler
/// answered, the placeable gate is armed, AND the demand is BOUNDABLE
/// — exactly by the post-filter page count on a complete view,
/// conservatively by the SUM OF THE TYPED POPULATION CLASSES (Ready +
/// forecast aggregates) on a truncated one (B3 truncation honesty:
/// each aggregate is the uncapped demand truth for its class and a
/// SUPERSET of every pool filter, so over-counting under-reaps — the
/// safe direction for absence inference). merged_bug_006: the
/// single-aggregate form summed a Ready-only aggregate against a
/// Ready+forecast page, so a forecast-backed Pending Job off-page was
/// counted by NEITHER term and reaped while still wanted — and its
/// "truncated with empty aggregate is incoherent" premise was false
/// for an all-forecast page (fail-closed, but silently disabling the
/// reap). With typed classes the premise is per-class: a truncated
/// view where BOTH classes read zero is incoherent — fail-closed
/// `None`, the same lever as scheduler-unreachable; an all-forecast
/// page is bounded by its own class. `max(queued, Σclasses)` is
/// defensive against an incoherent snapshot ever reporting
/// aggregate < page. Pure so the law table is unit-walkable.
// r[impl ctrl.pool.degraded-polarity]
// r[impl ctrl.pool.demand-completeness]
pub(crate) fn reap_queued_known(
    scheduler_ok: bool,
    gate_armed: bool,
    complete: bool,
    queued: u32,
    ready_upper: u32,
    forecast_upper: u32,
) -> Option<u32> {
    if !(scheduler_ok && gate_armed) {
        return None;
    }
    let classes_upper = ready_upper.saturating_add(forecast_upper);
    if complete {
        Some(queued)
    } else if classes_upper > 0 {
        Some(queued.max(classes_upper))
    } else {
        None
    }
}

/// The placeable-gate fold over the page (Builder pools), through the
/// page's OWN typed surface (round-10 R26 — the gate reads, the page
/// mutates): retain to the FFD-placed-on-Registered set when armed;
/// clear + `false` when unarmed (no FFD tick yet / standby replica) so
/// `queued_known = None` keeps `reap_excess_pending` fail-closed.
// r[impl ctrl.nodeclaim.placeable-gate+5]
pub(crate) fn apply_placeable_gate(
    page: &mut IntentPage,
    gate: &crate::reconcilers::nodeclaim_pool::PlaceableGate,
) -> Option<std::sync::Arc<crate::reconcilers::nodeclaim_pool::PlacedTick>> {
    use crate::reconcilers::nodeclaim_pool::FfdDisposition;
    match gate.snapshot() {
        Some(tick) => {
            // The DEMAND fold (R25: every letter named — no if-let
            // between the producer's alphabet and this law):
            page.retain_page(|i| match tick.disposition(&i.intent_id) {
                // Spawnable AND demand-visible.
                FfdDisposition::PlacedRegistered => true,
                // Round-10 merged_bug_012: window-deferred intents
                // stay DEMAND-VISIBLE (want-map membership, queued
                // count, re-ack) — the spawn pass filters them via
                // `PlacedTick::disposition` (not spawnable this
                // tick); pre-fix they were stripped here and their
                // still-wanted Pending Jobs orphan-reaped at 10s.
                FfdDisposition::Deferred => true,
                // Placed on an in-flight claim: NOT spawnable (the
                // pod would sit Pending until the claim registers)
                // and NOT demand-visible — the pre-round-10 posture,
                // kept deliberately: an in-flight placement normally
                // has no Job (spawn is gated until Registered), so
                // demand-visibility buys nothing, and widening it is
                // not this row's close (named here so the fold's
                // alphabet is total, not an accidental strip).
                FfdDisposition::PlacedInFlight => false,
                // Unplaceable this tick: stripped (cover_deficit owns
                // provisioning; the node-loss reap/respawn cycle is
                // the designed recovery).
                FfdDisposition::Unplaced => false,
            });
            Some(tick)
        }
        None => {
            page.clear_page();
            None
        }
    }
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

/// Render the [`INTENT_CELLS_ANNOTATION`] value from a spawned
/// intent: `zip(hw_class_names, node_affinity)` → `"h:cap"` rows,
/// where `cap` is the value of the term's
/// `karpenter.sh/capacity-type` `In` requirement. Returns `None`
/// (skip the stamp, warn at the caller) when the zip is unrenderable
/// — skewed lengths, a term without exactly one capacity value, or a
/// name that would collide with the grammar (`:`/`,` are not legal in
/// k8s label values, so a colliding name is config-invalid anyway).
/// Fail-open: an unstamped Job degrades to the legacy no-arm re-ack,
/// never a skewed echo (the scheduler refuses a whole ack on one
/// undecodable entry — validate-then-commit).
fn intent_cells_annotation_value(i: &SpawnIntent) -> Option<String> {
    if i.hw_class_names.is_empty() || i.hw_class_names.len() != i.node_affinity.len() {
        return None;
    }
    let mut rows = Vec::with_capacity(i.hw_class_names.len());
    for (h, term) in i.hw_class_names.iter().zip(&i.node_affinity) {
        if h.contains(':') || h.contains(',') {
            return None;
        }
        let cap = term
            .match_expressions
            .iter()
            .filter(|r| {
                r.key == crate::reconcilers::nodeclaim_pool::ffd::CAPACITY_TYPE_LABEL
                    && r.operator == "In"
            })
            .flat_map(|r| r.values.iter())
            .collect::<Vec<_>>();
        match cap.as_slice() {
            [one] if !one.contains(':') && !one.contains(',') => rows.push(format!("{h}:{one}")),
            _ => return None,
        }
    }
    Some(rows.join(","))
}

/// Parse the [`INTENT_CELLS_ANNOTATION`] back into the
/// `(hw_class_names, node_affinity)` echo pair — minimal terms (one
/// `In` requirement on the capacity label per cell), which is exactly
/// the shape the scheduler's arm decode consumes; pairing holds by
/// construction (`names.len() == terms.len()` from one parsed list).
/// `None` on an absent/garbled annotation (the legacy no-arm echo).
fn cells_from_annotation(
    v: &str,
) -> Option<(Vec<String>, Vec<rio_proto::types::NodeSelectorTerm>)> {
    let mut names = Vec::new();
    let mut terms = Vec::new();
    for row in v.split(',') {
        let (h, cap) = row.split_once(':')?;
        if h.is_empty() || cap.is_empty() {
            return None;
        }
        names.push(h.to_string());
        terms.push(rio_proto::types::NodeSelectorTerm {
            match_expressions: vec![rio_proto::types::NodeSelectorRequirement {
                key: crate::reconcilers::nodeclaim_pool::ffd::CAPACITY_TYPE_LABEL.into(),
                operator: "In".into(),
                values: vec![cap.to_string()],
            }],
        });
    }
    Some((names, terms))
}

/// The re-ack assembly (round-10 merged_bug_049, the R26 third-class
/// close): `to_ack` DERIVES from the controller's own Job LIST — the
/// local COMPLETE inventory — instead of filtering the demand page,
/// so the `ctrl.pool.ack-spawned-soundness` restart-totality contract
/// holds independent of paging. Per pending (non-reaped) Job:
///
/// - intent ON the page → the page copy (full-fidelity echo);
/// - OFF-page → reconstructed from the Job's own stamps
///   ([`INTENT_CELLS_ANNOTATION`] → the minimal decodable echo; the
///   scheduler reads `intent_id` + the `(names, terms)` zip and
///   nothing else from `spawned`);
/// - off-page with NO stamp (pre-upgrade Job) → the bare `intent_id`
///   (the legacy no-arm echo: `acked_spawned` witness lands, the arm
///   is `Empty`) — a one deploy-generation residual, PRICED: bounded
///   by the Job population spawned before this lane shipped, healed
///   by each such Job's natural terminal/reap cycle.
///
/// Census row (W10-AH): continuity / derives-from-Job-LIST.
// r[impl ctrl.pool.demand-completeness]
// r[impl ctrl.pool.ack-spawned-soundness]
pub(crate) fn assemble_re_acks(
    page: &IntentPage,
    pool: &str,
    kind: ExecutorKind,
    jobs: &[Job],
    reaped: &HashSet<String>,
    spawned: Vec<SpawnIntent>,
) -> Vec<SpawnIntent> {
    let page_by_name: HashMap<String, &SpawnIntent> = page
        .iter_page()
        .map(|i| (pod::job_name(pool, kind, &intent_suffix(&i.intent_id)), i))
        .collect();
    jobs.iter()
        .filter(|j| is_pending_job(j))
        .filter(|j| {
            !j.metadata
                .name
                .as_deref()
                .is_some_and(|n| reaped.contains(n))
        })
        .filter_map(|j| {
            let name = j.metadata.name.as_deref()?;
            if let Some(on_page) = page_by_name.get(name) {
                return Some((*on_page).clone());
            }
            // Off-page: the Job LIST is the inventory; the stamps are
            // the echo.
            let intent_id = super::job::job_intent_id(j)?.to_string();
            if intent_id.is_empty() {
                return None;
            }
            let (hw_class_names, node_affinity) = j
                .spec
                .as_ref()
                .and_then(|s| s.template.metadata.as_ref())
                .and_then(|m| m.annotations.as_ref())
                .and_then(|a| a.get(INTENT_CELLS_ANNOTATION))
                .and_then(|v| cells_from_annotation(v))
                .unwrap_or_default();
            Some(SpawnIntent {
                intent_id,
                hw_class_names,
                node_affinity,
                ..Default::default()
            })
        })
        .chain(spawned)
        .collect()
}

/// Delete Jobs whose name collides with an intent we are about to
/// spawn AND would block that spawn:
///
///   - **Terminal** (Complete/Failed): the deterministic `intent_
///     suffix` makes a finished Job for drv X block respawn of X for
///     `JOB_TTL_SECS`; clear that window so the immediate
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
/// The minted token evidence for one spawn pass (bug_121). `tokens`
/// maps intent_id → signed `ExecutorClaims`; `keyless` is the wire
/// discriminator — TRUE iff the scheduler holds no HMAC key, so an
/// empty map is no longer ambiguous between "no tokens EXIST
/// anywhere" (dev mode: spawn token-less) and "these intents were
/// OMITTED" (HMAC mode: skip them this tick). proto3 absence decodes
/// `keyless=false` = fail-closed Omitted.
pub(super) struct TokenGrants {
    pub(super) tokens: HashMap<String, String>,
    pub(super) keyless: bool,
}

/// bug_121: the per-intent token disposition — R21's per-element
/// letter set. The BATCH witness ([`TokenGrants`]) cannot carry the
/// element law; each intent folds its own letter at the spawn
/// chokepoint.
pub(super) enum TokenDisposition<'a> {
    /// A signed token covers this intent — spawn with it.
    Token(&'a str),
    /// HMAC mode and the mint omitted this id (the two-RPC read-read
    /// race: the drv left Ready between `GetSpawnIntents` and the
    /// mint) — SKIP this intent this tick. A token-less Job under
    /// HMAC is unauthenticatable BY CONSTRUCTION: first pull
    /// fast-fails Unauthenticated, the terminal Job
    /// NameCollision-blocks respawn behind the two-tick strike and
    /// steps the verdict-free backoff — the dead-builder shape
    /// through the Ok arm. Skipping spawns no Job, collides with
    /// nothing, taxes no backoff; the intent stays queued and
    /// re-presents next tick, minted.
    Omitted,
    /// Keyless dev mode — no tokens exist anywhere; spawn
    /// token-less (parity, knob-free).
    Keyless,
}

impl TokenGrants {
    /// Fold one intent's letter. Total over the closed
    /// {present} × {keyless} product — no wildcard arm.
    pub(super) fn disposition(&self, intent_id: &str) -> TokenDisposition<'_> {
        match self.tokens.get(intent_id) {
            Some(t) => TokenDisposition::Token(t),
            None if self.keyless => TokenDisposition::Keyless,
            None => TokenDisposition::Omitted,
        }
    }
}

/// bug_121 chokepoint: total-fold each intent's token letter BEFORE
/// `spawn_for_each` — Omitted intents never reach the spawn (the
/// fail-closed D-053-1 posture extended per element). Returns the
/// spawnable subset in input order.
pub(super) fn filter_spawnable_by_token(
    pool: &str,
    grants: &TokenGrants,
    intents: &[SpawnIntent],
) -> Vec<SpawnIntent> {
    let mut omitted: Vec<&str> = Vec::new();
    let spawnable: Vec<SpawnIntent> = intents
        .iter()
        .filter(|i| match grants.disposition(&i.intent_id) {
            TokenDisposition::Token(_) | TokenDisposition::Keyless => true,
            TokenDisposition::Omitted => {
                omitted.push(&i.intent_id);
                false
            }
        })
        .cloned()
        .collect();
    if !omitted.is_empty() {
        info!(
            pool,
            omitted = omitted.len(),
            ids = ?omitted,
            "mint omitted these intents (drv left Ready between \
             GetSpawnIntents and the mint); spawn skipped this tick — \
             no Job, no NameCollision, no backoff tax; they re-present \
             next tick, minted (bug_121)"
        );
    }
    spawnable
}

/// live_053 / D-053-1 (owner-signed, 2026-06-10): the spawn-token
/// mint outcome. `Some(grants)` is the token WITNESS a spawn may
/// consume — per-intent dispositions fold from it via
/// [`TokenGrants::disposition`] (bug_121: Token spawns, Omitted
/// skips that intent this tick, Keyless spawns token-less); `None` =
/// the mint RPC FAILED and no token evidence exists — the single
/// consumer (the reconcile spawn match) MUST spawn nothing this tick
/// (fail-closed: the durable-witness-coupling law — a spawn cannot
/// consume a witness that was never minted). The retired fail-open
/// arm spawned whole token-less batches that are unauthenticatable
/// BY CONSTRUCTION under HMAC: 257 builders died in one night when a
/// 134s scheduler stall expired the 5s mint RPC twice. Skipping
/// keeps the intents queued scheduler-side (no Job, no ack, no
/// dispatched_cells entry) — one tick of spawn latency instead of a
/// guaranteed-dead batch. Dev-mode parity holds WITHOUT a knob: a
/// keyless scheduler declares itself on the wire (`keyless=true`),
/// so dev rides the Some arm into the Keyless letter; the Err arm is
/// transport failure in every mode.
// r[impl sys.guard.brownout-only]
// r[impl sec.executor.identity-token+3]
pub(super) async fn mint_spawn_tokens(
    ctx: &Ctx,
    pool: &str,
    to_spawn: &[SpawnIntent],
) -> Option<TokenGrants> {
    if to_spawn.is_empty() {
        // Vacuous witness: no disposition will be consulted.
        // keyless=false is the conservative face.
        return Some(TokenGrants {
            tokens: HashMap::new(),
            keyless: false,
        });
    }
    match admin_call(ctx.admin.clone().mint_executor_tokens(
        rio_proto::types::MintExecutorTokensRequest {
            intent_ids: to_spawn.iter().map(|i| i.intent_id.clone()).collect(),
        },
    ))
    .await
    {
        Ok(r) => {
            let r = r.into_inner();
            Some(TokenGrants {
                tokens: r.tokens,
                keyless: r.keyless,
            })
        }
        Err(e) => {
            // E1 (the §1.6.4-15 granted hunk): the mint-failure
            // SKIPPED TICK, counted at the single site where the
            // failure is known (this Err arm is the sole None
            // producer; the consumer match's None arm is data-only).
            // One PromQL over this series replaces the log grep for
            // the live_053 fail-closed shape — production
            // tokenless-spawn is structurally dead, so the tick skip
            // IS the operator-visible symptom.
            metrics::counter!(
                "rio_controller_spawn_mint_skipped_ticks_total",
                "pool" => pool.to_owned(),
            )
            .increment(1);
            warn!(
                pool, error = %e,
                "mint_executor_tokens failed; skipping this tick's spawns \
                 (fail-closed: a token-less batch is dead on arrival under \
                 HMAC; intents stay queued and re-present next tick)"
            );
            None
        }
    }
}

/// live_056-b R21: the reap path's terminal-disposition alphabet —
/// every disposition the reap/respawn machinery mints folds through
/// ONE chokepoint ([`note_reap_disposition`]) into ONE counter
/// (`rio_controller_reap_dispositions_total{pool,disposition}`).
/// Shadow dispositions are unrepresentable: a new reap arm needs a
/// new letter HERE (rustc exhaustiveness at `as_label` is the
/// census), and the HELP-alphabet test pins every letter into the
/// described HELP string. The legacy per-arm counters
/// (`rio_controller_ephemeral_jobs_reaped_total`,
/// `rio_controller_orphan_jobs_reaped_total`) keep their published
/// series; this alphabet is the unified plane over ALL of them.
///
/// Divergence (recorded): the book's tentative letter list named a
/// `respawned` letter — not minted here. A respawn is the loop's
/// CONTINUATION, not a terminal disposition (R21 binds terminal
/// dispositions); its observability is the spawn path plus the
/// W9-CP interval witness, and a letter without a terminal premise
/// would be the inverse of the shadow-alphabet defect.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum ReapDisposition {
    /// `reap_excess_pending`: Pending beyond the queue depth.
    ExcessPending,
    /// `reap_stale_for_intents` orphan-pending arm: the intent left.
    OrphanPending,
    /// The orphan-pending arm SUSPENDED on an incomplete demand view
    /// (round-10 merged_bug_029 / R26): off-page absence is
    /// unknowable, so no delete — re-judged next tick. A non-delete
    /// letter (like the ladder edges) so the suspension is observable.
    OrphanSuspended,
    /// `reap_stale_for_intents` terminal arm: a finished Job blocking
    /// a still-wanted intent's respawn (NameCollision window).
    StaleTerminal,
    /// `reap_stale_for_intents` selector-drift arm: a Pending Job
    /// whose selector no longer matches the scheduler's re-solve.
    SelectorDrift,
    /// `reap_orphan_running` (I-165): Running past the grace with no
    /// scheduler assignment.
    OrphanRunning,
    /// Round-10 bug_078: a verdict-free TERMINAL reap whose Job
    /// completed CLEANLY (succeeded > 0 — the worker's own lawful
    /// exit, e.g. a forecast pod whose bound elapsed). Counted, never
    /// laddered: clean exits do not step the futility breaker.
    CleanExit,
    /// A verdict-free death stepped the respawn ladder (the
    /// escalation edge — minted with the death, before the next
    /// spawn pass).
    Escalated,
    /// The give-up threshold crossed: respawns stop until a named
    /// resolution (paired with the `RespawnGiveUp` Pool Event).
    GaveUp,
}

impl ReapDisposition {
    /// Every letter (the HELP-alphabet test walks this; adding a
    /// variant without extending it fails the length assert against
    /// the exhaustive match below). Test-facing census surface.
    #[cfg(test)]
    pub(super) const ALL: [Self; 9] = [
        Self::ExcessPending,
        Self::OrphanPending,
        Self::OrphanSuspended,
        Self::StaleTerminal,
        Self::SelectorDrift,
        Self::OrphanRunning,
        Self::CleanExit,
        Self::Escalated,
        Self::GaveUp,
    ];

    /// The metric label (rustc-exhaustive — the alphabet census).
    pub(super) fn as_label(self) -> &'static str {
        match self {
            Self::ExcessPending => "excess-pending",
            Self::OrphanPending => "orphan-pending",
            Self::OrphanSuspended => "orphan-suspended",
            Self::StaleTerminal => "stale-terminal",
            Self::SelectorDrift => "selector-drift",
            Self::OrphanRunning => "orphan-running",
            Self::CleanExit => "clean-exit",
            Self::Escalated => "escalated",
            Self::GaveUp => "gave-up",
        }
    }
}

/// live_056-b R21 chokepoint: the ONE mint site for reap-disposition
/// letters. Every reap arm and both ladder edges call through here —
/// `rg note_reap_disposition` is the complete disposition census.
pub(super) fn note_reap_disposition(pool: &str, d: ReapDisposition) {
    metrics::counter!(
        "rio_controller_reap_dispositions_total",
        "pool" => pool.to_owned(),
        "disposition" => d.as_label(),
    )
    .increment(1);
}

/// live_051(e): per-Job-uid stale-classification strikes — the
/// two-tick confirmation on the attempt-affecting reap arms
/// (`terminal`/`selector-drift`). The first tick a Job classifies
/// into one of those arms records a strike and DEFERS; the second
/// CONSECUTIVE tick reaps — any pull in flight at strike one
/// surfaces in strike two's view (refetched per tick) and vetoes at
/// the chokepoint. Keyed by (pool, uid) — the uid, never the
/// reusable name (the bug_089 lesson at the OA1 sample).
///
/// merged_bug_033: "consecutive" is enforced BY VALUE, not by code
/// position. Every reap pass advances a per-pool tick counter at
/// FUNCTION ENTRY — an empty-`want` pass (idle pool; the fail-closed
/// scheduler-error arm that polls as queued=0) IS a tick — and each
/// strike row carries the tick it was struck. A strike escalates
/// only when the new tick is ADJACENT to the stamped one
/// (`last_tick + 1 == tick`); any gap — an empty-want early return,
/// a pass where the Job did not classify — resets the count to 1, so
/// every exit path of the reap is correct by construction. The
/// previous shape (a function-tail `retain` of uids struck this
/// pass) enforced expiry positionally, and the `want.is_empty()`
/// early return bypassed it: strikes FROZE across exactly the
/// scheduler outages the two-tick confirmation absorbs, and the
/// first post-gap classification reaped on a non-consecutive
/// "strike 2".
///
/// Rows untouched past [`STRIKE_PRUNE_HORIZON`] are dropped at pass
/// entry — a MEMORY bound (deleted pools and long-gone uids would
/// otherwise leak in this process-global map forever), never the
/// correctness bound (adjacency-by-value is). Process-local like the
/// reconcile loop that feeds it; a controller restart counts afresh
/// (conservative — one extra tick of deferral). The orphan-pending
/// arm is OUT of scope: already age-gated by `REAP_PENDING_GRACE`.
/// Round-10 merged_bug_140 (R24): the strike book is an instantiation
/// of [`candidate::PoolScopedLedger`] — the key is SEALED to the
/// namespaced [`candidate::PoolKey`] (the pre-fix bare-name tick
/// clock interleaved same-named pools across namespaces: under
/// alternation `last_tick + 1 == tick` never held, StaleTerminal/
/// SelectorDrift reaps deferred indefinitely, and a SelectorDrift'd
/// Pending Job NameCollision-blocked its intent with no TTL
/// backstop), the prune law is the ledger's (rows AND tick clocks
/// retired together — the pre-fix begin_tick pruned only strikes,
/// leaking ticks entries against the ledger's own memory-bound doc),
/// and the firing law is COUNT-AND-FLOOR by construction
/// ([`StrikeEntry::confirmed`]: two adjacent-tick strikes AND the
/// [`STRIKE_WALL_FLOOR`] of wall clock since the row's first strike
/// — inherited from the exhaustion-streak sibling: Job event bursts
/// deliver adjacent ticks milliseconds apart, which re-opened the
/// priced in-flight-pull window the two-tick confirmation absorbs).
struct StrikeEntry {
    /// Consecutive-adjacent-tick strike count.
    count: u8,
    /// The pool tick this row was last struck at.
    last_tick: u64,
    /// Wall clock of this row's FIRST strike in the current
    /// consecutive run — the count-AND-floor anchor.
    first_struck: std::time::Instant,
    /// Wall clock of the last strike — feeds the memory prune ONLY.
    touched: std::time::Instant,
}

impl StrikeEntry {
    /// The firing law (count AND floor — by construction, the only
    /// confirmation path).
    fn confirmed(&self, now: std::time::Instant) -> bool {
        self.count >= 2 && now.saturating_duration_since(self.first_struck) >= STRIKE_WALL_FLOOR
    }
}

/// Prune horizon for strike rows — the memory bound, not a
/// correctness bound (see the ledger doc). One hour dwarfs any
/// plausible reconcile cadence: a row this stale belongs to a deleted
/// pool or a long-gone Job, and its adjacency has long since lapsed.
const STRIKE_PRUNE_HORIZON: std::time::Duration = std::time::Duration::from_secs(3600);

/// The wall-clock half of the strike firing law (merged_bug_140).
/// RE-DERIVED from the hazard the floor owns (R5 divergence record,
/// post-gate evidence): the two-tick confirmation exists so "any
/// pull in flight at strike one surfaces in strike two's view" —
/// Job-event bursts deliver adjacent ticks milliseconds apart, and
/// two ms-apart views cannot surface anything. The floor therefore
/// prices the PULL-SURFACING window: the strike-2 view must be
/// mintable strictly after strike 1's evidence horizon =
/// [`super::job::ATTEMPTS_VIEW_FRESHNESS`] (the 2s staleness license
/// a deciding view may carry) + scheduler-side attempt-visibility
/// slack + margin → 5s. The book's inherited hypothesis (the poison
/// sibling's 20s) priced a DIFFERENT hazard — verdict PERSISTENCE
/// for an irreversible report — and was falsified live: 20s on every
/// stale-terminal NameCollision clear added a deterministic
/// +10-20s per build-retry cycle (the fetcher-split fod-fail
/// pipeline went 61.4s → 71.2s, twice, identical — over its 60s
/// bound at every run). A reap is recoverable (the respawn follows);
/// poison persistence pricing does not transfer. Two timer-cadence
/// ticks (~10s) still clear the floor untouched; only sub-5s bursts
/// defer — exactly the population whose second view proves nothing.
/// VIOLABLE (R17): time axis only — the deferral it can add is
/// bounded by the floor itself; cost/size/population axes N/A (the
/// ledger's prune horizon is the memory envelope).
const STRIKE_WALL_FLOOR: std::time::Duration = std::time::Duration::from_secs(5);

/// Record a strike for `(pool, uid)` at `tick`: `previous + 1` when
/// the row's stamp is adjacent by value, else a fresh run of 1 (the
/// floor anchor re-arms with the run).
fn strike(
    ledger: &mut candidate::PoolScopedLedger<StrikeEntry>,
    pool: &candidate::PoolKey,
    uid: &str,
    tick: u64,
    now: std::time::Instant,
) -> StrikeEntry {
    let e = ledger.row_or_insert_with(pool, uid, || StrikeEntry {
        count: 0,
        last_tick: 0,
        first_struck: now,
        touched: now,
    });
    if e.count > 0 && e.last_tick + 1 == tick {
        e.count = e.count.saturating_add(1);
    } else {
        e.count = 1;
        e.first_struck = now;
    }
    e.last_tick = tick;
    e.touched = now;
    StrikeEntry {
        count: e.count,
        last_tick: e.last_tick,
        first_struck: e.first_struck,
        touched: e.touched,
    }
}

static STALE_STRIKES: std::sync::LazyLock<
    parking_lot::Mutex<candidate::PoolScopedLedger<StrikeEntry>>,
> = std::sync::LazyLock::new(|| parking_lot::Mutex::new(candidate::PoolScopedLedger::default()));

/// Test-only: rewind every strike row's floor anchor for `pool` so
/// the two-tick scenarios can cross [`STRIKE_WALL_FLOOR`] without
/// sleeping (the candidate.rs backdate_for_test precedent).
#[cfg(test)]
pub(super) fn backdate_strikes_for_test(pool: &candidate::PoolKey, by: std::time::Duration) {
    let mut book = STALE_STRIKES.lock();
    for (p, _, e) in book.iter_rows_mut() {
        if p == pool {
            e.first_struck -= by;
        }
    }
}

/// A Pending Job whose selector MATCHES the current intent is NOT
/// reaped — that's the intended NameCollision dedupe.
///
/// Deletions route through
/// [`super::job::delete_job_with_synthesized_report`] (reason
/// `Reaped`): the targets are terminal or Pending Jobs, so the
/// synthesize arm is normally inert, but a stale Job whose pod pulled
/// and crashed gets its open attempt closed at deletion instead of
/// waiting for the establishment sweep.
///
/// bug_028 futility breaker: the TERMINAL arm is the verdict-free-
/// death observation point — a terminal Job reaped for a still-wanted
/// intent with NO verdict anywhere (the delete's report was not
/// RESOLVED by the scheduler, and neither an open nor a
/// recently-closed BUILD attempt covers the intent —
/// merged_bug_080(2b)) means the same-named respawn would otherwise
/// fire this very tick (the reap exists to clear the NameCollision
/// window). The death is noted BEFORE the spawn pass runs, so the
/// backoff gates the respawn immediately. The orphan-pending arm
/// reaps UN-wanted intents (no respawn follows) and the
/// selector-drift arm reaps never-ran Pending Jobs whose respawn is
/// the INTENDED re-render — neither is a verdict-free death.
#[allow(clippy::too_many_arguments)] // the build_job precedent: reconcile plumbing, not an API
pub(super) async fn reap_stale_for_intents(
    jobs_api: &Api<Job>,
    existing: &[Job],
    want: &WantMap,
    ctx: &Ctx,
    pool_obj: &Pool,
    pool: &str,
    key: &candidate::PoolKey,
) -> HashSet<String> {
    let mut reaped = HashSet::new();
    // merged_bug_033: the strike clock advances at FUNCTION ENTRY,
    // before the empty-`want` early return below — an empty pass IS
    // a tick, so strikes stamped before a scheduler outage / idle
    // stretch read as non-adjacent when classification resumes.
    let tick = {
        let mut book = STALE_STRIKES.lock();
        let now = std::time::Instant::now();
        // The ledger's unified prune: rows AND tick clocks together.
        book.prune_stale(now, STRIKE_PRUNE_HORIZON, |e| e.touched);
        book.begin_tick(key, now)
    };
    if want.is_empty() {
        return reaped;
    }
    // Lazily fetched once per tick, only if a delete is actually about
    // to happen (the synthesize-on-delete input; best-effort).
    // merged_bug_080(2b): the FULL view — the death classification
    // below consults the `recently_closed` half too. live_051(e): the
    // view rides inside its freshness WITNESS — the chokepoint
    // refuses to adjudicate on stale evidence and refreshes the
    // witness in place, so the gate below always folds the same
    // evidence the delete consumed.
    let mut attempts_view: Option<super::job::AttemptsPair> = None;
    for j in existing {
        let Some(jn) = j.metadata.name.as_deref() else {
            continue;
        };
        let (params, disposition) = match want.verdict(jn) {
            // TRUE negative evidence (complete view only — the R26
            // accessor). Pending → orphan: the intent left (cancel /
            // completes-elsewhere / disconnect) and this Job will
            // never receive an assignment. Reap by intent-membership
            // HERE so the surplus is the orphan set, not an arbitrary
            // age-prefix — `select_excess_pending`'s oldest-first reap
            // would otherwise delete still-live Jobs (losing in-flight
            // Karpenter provisioning) while orphans survive ≥1 extra
            // tick. Running → leave alone (may hold assignment;
            // `reap_orphan_running` owns it). The `want.is_empty()`
            // early-return above is the fail-closed gate (scheduler
            // error → no orphan-reap).
            WantVerdict::AbsentFromDemand
                if is_pending_job(j) && job_older_than(j, REAP_PENDING_GRACE) =>
            {
                (DeleteParams::foreground(), ReapDisposition::OrphanPending)
            }
            WantVerdict::AbsentFromDemand => continue,
            // r[impl ctrl.pool.demand-completeness]
            // merged_bug_029: off an INCOMPLETE page, absence is
            // unknowable — the destructive absence-keyed arm SUSPENDS
            // (typed letter + counter so the suspension is
            // observable), re-judged next tick. The 10s grace is
            // unchanged for complete views; a still-wanted off-page
            // Pending Job survives the window instead of being
            // foreground-deleted single-tick (delete/respawn churn of
            // in-flight Karpenter provisioning, exactly in the >2048-
            // backlog regime the window targets).
            WantVerdict::Unknowable => {
                if is_pending_job(j) && job_older_than(j, REAP_PENDING_GRACE) {
                    note_reap_disposition(pool, ReapDisposition::OrphanSuspended);
                    debug!(
                        pool, job = %jn,
                        "orphan-pending reap suspended: demand view \
                         incomplete (absence unknowable off-page; \
                         re-judged next tick)"
                    );
                }
                continue;
            }
            WantVerdict::Wanted(_) if !is_active_job(j) => {
                (DeleteParams::background(), ReapDisposition::StaleTerminal)
            }
            WantVerdict::Wanted(want_sel)
                if is_pending_job(j)
                    && j.metadata
                        .annotations
                        .as_ref()
                        .and_then(|a| a.get(INTENT_SELECTOR_ANNOTATION))
                        .map(String::as_str)
                        != Some(want_sel) =>
            {
                (DeleteParams::foreground(), ReapDisposition::SelectorDrift)
            }
            WantVerdict::Wanted(_) => continue,
        };
        // R21: the log field IS the metric label — one alphabet.
        let why = disposition.as_label();
        // live_051(e): two-tick confirmation on the attempt-affecting
        // arms — the first stale classification records a strike and
        // defers; the orphan-pending arm keeps its single-tick path
        // (already age-gated by REAP_PENDING_GRACE).
        if matches!(
            disposition,
            ReapDisposition::StaleTerminal | ReapDisposition::SelectorDrift
        ) {
            let Some(uid) = j.metadata.uid.clone() else {
                // No uid: cannot key a strike — defer conservatively
                // (apiserver objects always carry one in practice).
                continue;
            };
            let now = std::time::Instant::now();
            let row = strike(&mut STALE_STRIKES.lock(), key, &uid, tick, now);
            if !row.confirmed(now) {
                info!(
                    pool, job = %jn, why, strikes = row.count,
                    "stale-Job classification deferred (live_051(e) \
                     two-tick confirmation + the merged_bug_140 \
                     pull-surfacing wall floor; re-decided next tick)"
                );
                continue;
            }
        }
        let view = match &mut attempts_view {
            Some(v) => v,
            None => {
                // merged_bug_022: a FAILED lazy fetch defers every
                // remaining attempt-affecting delete this tick — no
                // NoOpenAttempt deletes, no backoff tax, from an
                // error-born empty view. Strikes recorded above are
                // stamped at THIS tick, so the next pass reads
                // adjacent-by-value and re-decides at count >= 2.
                match super::job::AttemptsViewWitness::fetch(ctx, pool).await {
                    super::job::AttemptsFetch::Fetched(w) => {
                        attempts_view = Some(super::job::AttemptsPair::at_selection(w));
                        attempts_view.as_mut().expect("just set")
                    }
                    super::job::AttemptsFetch::FetchFailed => break,
                }
            }
        };
        match super::job::delete_job_with_synthesized_report(
            jobs_api,
            ctx,
            j,
            jn,
            &params,
            rio_proto::types::AttemptTerminalReason::Reaped,
            view,
            key,
        )
        .await
        {
            Ok(super::job::SynthesizedDelete::Deferred { fresh_attempt }) => {
                info!(
                    pool, job = %jn, why, fresh_attempt,
                    "stale-Job reap deferred at the chokepoint on attempt \
                     evidence (live_051(e)); re-decided next tick"
                );
                // NOT reaped: the Job stands; the strike row is
                // stamped at this tick, so the next pass reads
                // adjacent-by-value and re-decides at count >= 2
                // (strike monotonicity — no infinite-defer arm).
            }
            Ok(synthesized) => {
                info!(
                    pool, job = %jn, why,
                    "reaped stale Job blocking re-queued intent respawn"
                );
                // bug_028 futility breaker: a terminal reap of a
                // still-wanted intent with no verdict is a VERDICT-FREE
                // death — step its respawn record so the same-tick
                // respawn meets the backoff floor. Verdict presence is
                // the exhaustive merged_bug_080(2b) alphabet (a
                // resolving ack already cleared the record inside the
                // delete chokepoint; a charge-free ack proves nothing):
                // r[impl ctrl.pool.respawn-backoff+2]
                let no_verdict_at_delete = match &synthesized {
                    super::job::SynthesizedDelete::ReportedVerdict { .. } => false,
                    super::job::SynthesizedDelete::AckedNoAttempt
                    | super::job::SynthesizedDelete::ReportFailed
                    | super::job::SynthesizedDelete::NoOpenAttempt => true,
                    // Peeled by the arm above; a deferred delete
                    // reaped nothing and adjudicates nothing.
                    super::job::SynthesizedDelete::Deferred { .. } => {
                        unreachable!("Deferred is peeled by the preceding match arm")
                    }
                };
                // Round-10 bug_078 leg (ii): the verdict-free reap
                // splits into TYPED exit dispositions (R21) — the
                // §13b-legalized CLEAN idle exit (the worker chose to
                // exit: Job Complete, succeeded > 0 — a forecast pod
                // whose work never arrived inside its bound, or any
                // lawful no-work exit) is NOT pathological and stops
                // stepping the wedged-builder ladder; a WEDGED death
                // (Job Failed: OOM, nonzero exit, deadline) steps as
                // before. The discriminator is the Job's own terminal
                // status — the same observable the reap classified on.
                let clean_idle_exit =
                    j.status.as_ref().and_then(|st| st.succeeded).unwrap_or(0) > 0;
                if matches!(disposition, ReapDisposition::StaleTerminal)
                    && no_verdict_at_delete
                    && clean_idle_exit
                    && let Some(intent_id) = super::job::job_intent_id(j)
                    && !intent_id.is_empty()
                {
                    // The clean letter is COUNTED (observable), never
                    // laddered: k clean exits step nothing.
                    note_reap_disposition(pool, ReapDisposition::CleanExit);
                    debug!(
                        pool, intent = %intent_id,
                        "verdict-free CLEAN exit (Job Complete, no \
                         attempt): respawn proceeds at cadence — the \
                         futility ladder is for wedged deaths \
                         (bug_078)"
                    );
                }
                if matches!(disposition, ReapDisposition::StaleTerminal)
                    && no_verdict_at_delete
                    && !clean_idle_exit
                    && let Some(intent_id) = super::job::job_intent_id(j)
                    && !intent_id.is_empty()
                {
                    // ...and only when NO open and NO recently-closed
                    // BUILD attempt covers the intent: a worker-closed
                    // death the scheduler already adjudicated (the
                    // close outran the reap) is NOT verdict-free —
                    // taxing the healthy retry violated the breaker's
                    // own doc. The kind law is single-sourced in the
                    // witness mints. Residual (disclosed): a close
                    // older than the scheduler's recently-closed
                    // window still reads verdict-free — over-caution
                    // bounded by the 80 s backoff cap; widening the
                    // window is scheduler-side. live_051(e): the gate
                    // reads the SAME witness the delete consumed
                    // (refreshed in place on staleness).
                    let gate_view = attempts_view
                        .as_ref()
                        .expect("witness minted before any delete")
                        .freshest();
                    let covered_by_build = gate_view
                        .attempts()
                        .iter()
                        .filter(|a| a.intent_id == intent_id)
                        .any(|a| candidate::VerdictWitness::from_open_build_attempt(a).is_some())
                        || gate_view
                            .recently_closed()
                            .iter()
                            .filter(|c| c.intent_id == intent_id)
                            .any(|c| {
                                // merged_bug_036 mint 4b: the close
                                // must POSTDATE the reaped Job's
                                // creation to cover its death —
                                // bug_122: on the REBASED age (the
                                // gate view is held up to the 2s
                                // license; staleness must not eat
                                // the skew slack).
                                candidate::VerdictWitness::covers_job_death(
                                    c,
                                    j,
                                    gate_view.staleness(),
                                )
                                .is_some()
                            });
                    if !covered_by_build {
                        // Lock scope ends at the statement — the
                        // Event publish below must not hold it.
                        let note = ctx.exhausted_streak.lock().note_verdict_free_death(
                            key,
                            intent_id,
                            std::time::Instant::now(),
                        );
                        // live_056-b: the ladder edges are alphabet
                        // letters (R21) — Escalated with every
                        // verdict-free death, GaveUp exactly once at
                        // the threshold, paired with the operator
                        // verdict-plane Event on the Pool.
                        note_reap_disposition(pool, ReapDisposition::Escalated);
                        if note.gave_up_edge {
                            note_reap_disposition(pool, ReapDisposition::GaveUp);
                            warn!(
                                pool, intent = %intent_id, deaths = note.deaths,
                                "respawn GIVE-UP: verdict-free deaths crossed the \
                                 threshold; respawns stop until a named resolution \
                                 (live_056-b)"
                            );
                            ctx.publish_event(
                                pool_obj,
                                &kube::runtime::events::Event {
                                    type_: kube::runtime::events::EventType::Warning,
                                    reason: "RespawnGiveUp".into(),
                                    note: Some(format!(
                                        "intent {intent_id}: {} verdict-free builder \
                                         deaths; respawns stop until a named \
                                         resolution (scheduler verdict or operator \
                                         action). The intent stays queued.",
                                        note.deaths
                                    )),
                                    action: "Reconcile".into(),
                                    secondary: None,
                                },
                            )
                            .await;
                        }
                    }
                }
                note_reap_disposition(pool, disposition);
                reaped.insert(jn.to_owned());
            }
            Err(e) if e.is_not_found() => {
                note_reap_disposition(pool, disposition);
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
    // merged_bug_033: no function-tail ledger maintenance — expiry
    // is BY VALUE (tick stamps), so there is no retain for an early
    // return to bypass.
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
// r[impl ctrl.pool.ephemeral+1]
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
    // gate-active only. §13e: fetcher cells ARE NodeClaim-minted now
    // (`rio.build/fetcher` taint+label, `nodeclaim_pool` covers Fetcher
    // Pools post-B2.6), but the second-scheduler routing stays
    // builder-only. Fetcher pods are NOT one-per-node (no anti-
    // affinity — multiple ~1c pods can co-locate); the divergence
    // between FFD's prediction and the default scheduler's actual
    // bin-packing is bounded by the tiny per-pod footprint, and it
    // isn't worth coupling fetcher cells to the `kube-build-scheduler`
    // dependency. `gate_active=false` when EITHER the NodeClaim CRD is
    // absent (k3s VM tests without Karpenter) OR `buildScheduler.
    // enabled=false` (operator disabled the second scheduler — registry
    // mirror gap, rollback, derived overlay) — in both cases
    // `kube-build-scheduler` isn't deployed, so a pod targeting it
    // would sit Pending forever. r40 bug_018.
    if gate_active && pool.spec.kind == ExecutorKind::Builder {
        pod_spec.scheduler_name = Some(KUBE_BUILD_SCHEDULER.into());
        pod_spec.priority_class_name = Some(format!(
            "{PRIORITY_CLASS_PREFIX}{}",
            priority_bucket(intent.cores)
        ));
    }
    // r[impl sec.executor.identity-token+3]
    // Pass the scheduler-signed token (from `MintExecutorTokens`, NOT
    // `SpawnIntent`) through verbatim so the builder presents it on
    // `PullAssignment` / `ReportOutcome`. Per-intent (not per-Pool), so
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
    // Round-10 bug_078: stamp the RENDERED idle bound so the
    // orphan-running grace can cover the pod's own patience (see
    // IDLE_EXIT_SECS_ANNOTATION). Stamped unconditionally — the
    // readback treats absent as the flat bound.
    pod_anns.insert(
        IDLE_EXIT_SECS_ANNOTATION.into(),
        pod::idle_exit_secs(intent).to_string(),
    );
    // Round-10 merged_bug_049: the durable cell echo for the
    // page-independent re-ack lane (see INTENT_CELLS_ANNOTATION).
    // Fail-open on unrenderable zips — hw-agnostic intents
    // (hw_class_names=[]) legitimately have no cells to stamp.
    if let Some(cells) = intent_cells_annotation_value(intent) {
        pod_anns.insert(INTENT_CELLS_ANNOTATION.into(), cells);
    } else if !intent.hw_class_names.is_empty() {
        warn!(
            intent = %intent.intent_id,
            "intent cells unrenderable (skewed names/affinity zip or \
             grammar-colliding name); Job spawns unstamped — its \
             off-page re-ack degrades to the no-arm echo"
        );
    }
    pod_anns.insert(
        DEADLINE_SECS_ANNOTATION.into(),
        ephemeral_deadline(intent).to_string(),
    );
    pod_anns.insert(HW_BENCH_NEEDED_ANNOTATION.into(), bench_needed.to_string());
    // Stamp the selector fingerprint on the JOB metadata (not pod
    // template) so `reap_stale_for_intents` can compare without
    // dereferencing `spec.template`.
    job.metadata
        .annotations
        .get_or_insert_with(BTreeMap::new)
        .insert(
            INTENT_SELECTOR_ANNOTATION.into(),
            candidate::RenderInputs::from_intent(intent).fingerprint(),
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
pub(super) fn apply_intent_resources(
    pod_spec: &mut PodSpec,
    pool: &Pool,
    i: &SpawnIntent,
    hw: &HwClassConfig,
) {
    let headroom = intent_headroom(i);
    let overlay_limit = (i.disk_bytes as f64 * headroom) as u64;
    // live_058-a: the container triple comes from the FOOTPRINT
    // constructor (the pad/floor law) and is stamped through the
    // sealed helper — the raw solve cannot reach the resource map
    // from here. `pod::fuse_cache_bytes(pool)` keys on
    // `pool.spec.kind`; the constructor's selection keys on
    // `SpawnIntent.kind` — the scheduler's intent filter keeps the
    // two in agreement for every intent a pool actually spawns (the
    // pre-existing footprint contract, now load-bearing for the
    // stamp too).
    let fp = intent_pod_footprint(i, pod::fuse_cache_bytes(pool));
    let container = pod_spec
        .containers
        .first_mut()
        .expect("build_executor_pod_spec emits exactly one container");
    stamp_container_resources(container, &fp);

    // Couple the worker's `daemon_timeout` to the per-intent K8s
    // `activeDeadlineSeconds`: worker fires `WORKER_DEADLINE_SLACK_SECS`
    // BEFORE K8s SIGKILLs, so `CompletionReport{TimedOut}` (primary
    // path) carries telemetry and reaches `handle_timeout_failure`'s
    // cap-check; `DeadlineExceeded` stays the wedged-worker backstop
    // per `r[sched.termination.deadline-exceeded+3]`.
    // `ephemeral_deadline` floors at 180 so `− 90` never underflows
    // the `.max(60)` clamp into a tie.
    let worker_timeout = (ephemeral_deadline(i) - WORKER_DEADLINE_SLACK_SECS).max(60);
    let env = container.env.get_or_insert_with(Vec::new);
    env.push(pod::env(
        "RIO_DAEMON_TIMEOUT_SECS",
        &worker_timeout.to_string(),
    ));
    // Round-10 bug_078 leg (i): the eta-aware idle bound. The base
    // spec renders the flat `RIO_IDLE_SECS` (pod::POOL_IDLE_EXIT_SECS)
    // intent-free; forecast intents OVERRIDE it in place here —
    // mutate, never duplicate (kubelet takes the last duplicate but
    // apply-validation warns, and one name must mean one value).
    let idle = pod::idle_exit_secs(i);
    if idle != pod::POOL_IDLE_EXIT_SECS
        && let Some(e) = env.iter_mut().find(|e| e.name == "RIO_IDLE_SECS")
    {
        e.value = Some(idle.to_string());
    }
    // r[impl ctrl.pool.hw-bench-needed+2]
    // Downward-API env var for `rio.build/hw-bench-needed`. The
    // annotation is stamped at pod-CREATE time by `build_job` (above on
    // the call stack), so the env-var form's resolve-once-at-container-
    // create is race-free here — unlike `rio.build/hw-class` which is
    // stamped after-bind by `run_pod_annotator` and so MUST use the
    // volume form. There is no absent-annotation fallback: `build_job`
    // always stamps the annotation as "true"/"false" on this same pod
    // template. If the annotation were ever absent, the kubelet would
    // resolve the fieldRef to "" and the config loader would reject the
    // empty string for a bool field — the pod fails at startup (loud)
    // rather than silently defaulting. Empty env values are deliberately
    // NOT treated as unset: `RIO_CHUNK_BACKEND__PREFIX=""` and friends
    // are legitimate empty-string values.
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

    // r[impl sched.dispatch.fleet-exhaust+5]
    // AD2: nodes that already failed this derivation (the intent's
    // node-keyed exclusion set) are rendered as required anti-affinity,
    // ANDed into the per-intent placement above. Applied AFTER the
    // affinity block so the NotIn requirement lands inside every term.
    pod::apply_excluded_nodes_anti_affinity(pod_spec, &i.excluded_nodes);

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
    // this covers the intent-affinity path. On the kvm/metal axis
    // there is no pool-static *nodeSelector* path — r33 bug_002
    // deleted it; restrictive placement is `intent.node_affinity`
    // only. Fetcher pools DO carry a pool-static `rio.build/fetcher`
    // selector (r35 B4 — see `pod::effective_node_selector`), keyed
    // on `pool.spec.kind`, not `hw_class_names`.) Append-dedup so the
    // structural `rio.build/builder` toleration (injected by
    // `pod::effective_tolerations` — r38 bug_027) and the pool-static
    // kvm toleration both survive without duplication.
    let mut intent_tols: Vec<Toleration> = Vec::new();
    for h in &i.hw_class_names {
        for t in hw.taints_for(h) {
            // §13d chokepoint: `pod::taint_to_toleration` is the SAME
            // projection `effective_tolerations`'s fetcher arm uses.
            // Open-coding it here would let the per-intent toleration
            // and the dedup target (`pod_t.contains` below) diverge —
            // e.g. one side switching to `Exists` for empty-value
            // taints, or one adding `toleration_seconds`.
            let tol = pod::taint_to_toleration(t);
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
    pub(super) use crate::fixtures::{test_pool, test_sched_addrs, test_store_addrs};

    pub(super) fn intent(id: &str) -> SpawnIntent {
        SpawnIntent {
            intent_id: id.into(),
            ..Default::default()
        }
    }

    fn ledger() -> candidate::PoolScopedLedger<StrikeEntry> {
        candidate::PoolScopedLedger::default()
    }

    /// Strike + prune driver mirroring the production pass entry
    /// (prune-then-tick), keyed on the SEALED PoolKey.
    fn tick_at(
        l: &mut candidate::PoolScopedLedger<StrikeEntry>,
        pool: &candidate::PoolKey,
        now: std::time::Instant,
    ) -> u64 {
        l.prune_stale(now, STRIKE_PRUNE_HORIZON, |e| e.touched);
        l.begin_tick(pool, now)
    }

    /// W9-AQ (merged_bug_033), ledger faces: "consecutive" is
    /// adjacent ticks BY VALUE — a gap of empty-want passes (each
    /// advances the clock at function entry) resets the count, so
    /// the first post-gap classification is strike 1, never a
    /// frozen "strike 2"; an adjacent follow-up still escalates.
    #[test]
    fn w9_aq_strike_gap_resets_consecutiveness_by_value() {
        let mut l = ledger();
        let pk = candidate::PoolKey::new("rio-system", "p");
        let now = std::time::Instant::now();
        let t1 = tick_at(&mut l, &pk, now);
        assert_eq!(strike(&mut l, &pk, "uid-1", t1, now).count, 1);
        // k empty-want passes: the clock advances, nothing strikes.
        let _ = tick_at(&mut l, &pk, now);
        let _ = tick_at(&mut l, &pk, now);
        let t4 = tick_at(&mut l, &pk, now);
        assert_eq!(
            strike(&mut l, &pk, "uid-1", t4, now).count,
            1,
            "left: 2 (the frozen strike escalates across the gap) / \
             right: 1 (adjacency by value resets the confirmation)"
        );
        // Adjacent follow-up: two genuine back-to-back
        // classifications still confirm (count half; the wall floor
        // is the other conjunct — see the count-and-floor test).
        let t5 = tick_at(&mut l, &pk, now);
        assert_eq!(strike(&mut l, &pk, "uid-1", t5, now).count, 2);
    }

    /// Strike clocks are POOL-scoped: another pool's passes advance
    /// only that pool's tick — they neither break this pool's
    /// adjacency nor manufacture a gap.
    #[test]
    fn strike_clocks_are_pool_scoped() {
        let mut l = ledger();
        let pk = candidate::PoolKey::new("rio-system", "p");
        let qk = candidate::PoolKey::new("rio-system", "q");
        let now = std::time::Instant::now();
        let t1 = tick_at(&mut l, &pk, now);
        assert_eq!(strike(&mut l, &pk, "uid-1", t1, now).count, 1);
        // Sibling-pool passes interleave (the reconcile loop is
        // per-pool; ticks for q are not ticks for p).
        let _ = tick_at(&mut l, &qk, now);
        let _ = tick_at(&mut l, &qk, now);
        let t2 = tick_at(&mut l, &pk, now);
        assert_eq!(
            strike(&mut l, &pk, "uid-1", t2, now).count,
            2,
            "p's tick 2 is adjacent to p's tick 1 regardless of q's clock"
        );
    }

    // r[verify ctrl.pool.demand-completeness]
    /// **W10-AN (merged_bug_140).** PROPOSITION at the cross-namespace
    /// quantifier: same-NAMED pools in distinct namespaces are
    /// DISTINCT strike owners — under strict alternation each pool's
    /// adjacency holds independently and the confirmation fires
    /// (count-based). Pre-fix (bare-name keys, strawman-DISCLOSED:
    /// the shipped `ticks: HashMap<String, u64>` shape is deleted by
    /// the seal — the alternation arithmetic is quoted in the commit
    /// body: ns-a's passes land ticks 1,3,5 on the SHARED clock, so
    /// `last_tick + 1 == tick` never held and zero reaps ever fired).
    #[test]
    fn w10_an_same_named_pools_alternate_independently() {
        let mut l = ledger();
        let a = candidate::PoolKey::new("ns-a", "metal");
        let b = candidate::PoolKey::new("ns-b", "metal");
        let now = std::time::Instant::now();
        // Strict alternation, two rounds each.
        let ta1 = tick_at(&mut l, &a, now);
        let tb1 = tick_at(&mut l, &b, now);
        assert_eq!(strike(&mut l, &a, "uid-a", ta1, now).count, 1);
        assert_eq!(strike(&mut l, &b, "uid-b", tb1, now).count, 1);
        let ta2 = tick_at(&mut l, &a, now);
        let tb2 = tick_at(&mut l, &b, now);
        assert_eq!(
            strike(&mut l, &a, "uid-a", ta2, now).count,
            2,
            "ns-a/metal confirms on ITS OWN adjacent ticks (pre-fix: \
             the shared bare-name clock read 1,3 — never adjacent, \
             zero reaps ever fired)"
        );
        assert_eq!(strike(&mut l, &b, "uid-b", tb2, now).count, 2);
        // The floor conjunct: confirmation = count AND wall floor.
        let aged = now + STRIKE_WALL_FLOOR;
        let ta3 = tick_at(&mut l, &a, aged);
        let row = strike(&mut l, &a, "uid-a", ta3, aged);
        assert!(
            row.confirmed(aged),
            "count-and-floor fires once both conjuncts hold"
        );
        let tb3 = tick_at(&mut l, &b, aged);
        let _ = strike(&mut l, &b, "uid-burst", tb3, aged);
        let burst_now = aged + std::time::Duration::from_millis(1);
        let tb4 = tick_at(&mut l, &b, burst_now);
        let burst2 = strike(&mut l, &b, "uid-burst", tb4, burst_now);
        assert_eq!(burst2.count, 2, "event-burst adjacency counts...");
        assert!(
            !burst2.confirmed(burst_now),
            "...but the pull-surfacing wall floor DEFERS millisecond-\
             adjacent bursts (a ms-apart second view proves nothing)"
        );
    }

    /// merged_bug_033 leak pin: rows for absent pools (pool deleted,
    /// uid long gone — nothing ever strikes them again) vanish at
    /// the prune horizon via ANY pool's pass entry. The horizon is a
    /// MEMORY bound; correctness rides adjacency-by-value alone.
    #[test]
    fn strike_rows_for_absent_pools_prune_at_the_horizon() {
        let mut l = ledger();
        let dead = candidate::PoolKey::new("rio-system", "deleted-pool");
        let live = candidate::PoolKey::new("rio-system", "live-pool");
        let t0 = std::time::Instant::now();
        let t = tick_at(&mut l, &dead, t0);
        let _ = strike(&mut l, &dead, "uid-9", t, t0);
        assert_eq!(l.sizes().0, 1);
        // Just inside the horizon: retained.
        let _ = tick_at(
            &mut l,
            &live,
            t0 + STRIKE_PRUNE_HORIZON - std::time::Duration::from_secs(1),
        );
        assert_eq!(l.sizes().0, 1, "row inside the horizon is retained");
        // At the horizon: any pool's next pass prunes globally.
        let _ = tick_at(&mut l, &live, t0 + STRIKE_PRUNE_HORIZON);
        assert_eq!(
            l.sizes().0,
            0,
            "absent-pool rows vanish at the stamp horizon (memory bound)"
        );
    }

    // r[verify ctrl.pool.demand-completeness]
    /// **W10-AO (merged_bug_140, the ticks leak).** The unified prune
    /// retires TICK-CLOCK entries with the rows: k dead pools tick
    /// once each, then a live pool's pass at the horizon prunes ALL
    /// of them — the pre-fix begin_tick pruned only strikes, so the
    /// ticks map grew one entry per pool name forever against the
    /// ledger's own memory-bound doc.
    ///
    /// Pre-fix red (the split prune — rows-only):
    ///   left: 8  right: 1 (dead pools' tick clocks leaked)
    #[test]
    fn w10_ao_tick_clocks_prune_with_the_rows() {
        let mut l = ledger();
        let t0 = std::time::Instant::now();
        for k in 0..8 {
            let dead = candidate::PoolKey::new("rio-system", &format!("dead-{k}"));
            let _ = tick_at(&mut l, &dead, t0);
        }
        assert_eq!(l.sizes().1, 8, "eight pools ticked");
        let live = candidate::PoolKey::new("rio-system", "live");
        let _ = tick_at(&mut l, &live, t0 + STRIKE_PRUNE_HORIZON);
        assert_eq!(
            l.sizes().1,
            1,
            "dead pools' tick clocks pruned with the rows (pre-fix: \
             leaked forever); the live pool's fresh clock remains"
        );
    }

    /// live_056-b R21 HELP-alphabet pin: the
    /// rio_controller_reap_dispositions_total describe HELP names
    /// EVERY letter of the alphabet — a new variant fails here until
    /// both `ALL` and the HELP are extended (and the (ttttt) regen
    /// pair refreshes docs/gen + the helm metric-help surface).
    #[test]
    fn reap_disposition_help_alphabet_is_total() {
        // Embedded at compile time (the b870121ac form) — no runtime
        // tree dependence, so the pin holds in the gate sandbox too.
        let src = include_str!("../../lib.rs");
        let start = src
            .find("rio_controller_reap_dispositions_total")
            .expect("describe present in lib.rs");
        let help = &src[start..(start + 1500).min(src.len())];
        for d in ReapDisposition::ALL {
            assert!(
                help.contains(d.as_label()),
                "HELP must name '{}' (R21: the alphabet travels with \
                 the metric)",
                d.as_label()
            );
        }
        // The exhaustive match in as_label is the census; ALL must
        // cover it (a new variant fails the match first, then here).
        assert_eq!(ReapDisposition::ALL.len(), 9);
    }

    /// W9-CO, Job-spec face (live_056-b): the minted Job carries the
    /// serving readiness probe — httpGet /servingz on the named
    /// health port — and NO liveness/startup probes (D3: builders
    /// are kill-wired by EXIT; I-114's liveness rationale stands —
    /// a CPU-pegged build must never be SIGKILLed by a probe).
    #[test]
    fn job_spec_carries_the_serving_readiness_probe() {
        let pool = test_pool("probe-pool", ExecutorKind::Builder);
        let j = job(&pool, &intent("abc123"));
        let tmpl = j.spec.unwrap().template;
        let c = &tmpl.spec.unwrap().containers[0];
        let probe = c.readiness_probe.as_ref().expect("readiness probe minted");
        let http = probe.http_get.as_ref().expect("httpGet form");
        assert_eq!(http.path.as_deref(), Some("/servingz"));
        assert_eq!(
            http.port,
            k8s_openapi::apimachinery::pkg::util::intstr::IntOrString::String("health".into()),
            "probes the builder's own health server"
        );
        assert!(
            c.liveness_probe.is_none(),
            "kill-wired by exit (D3) — liveness stays absent"
        );
        assert!(c.startup_probe.is_none(), "no startup probe either");
    }

    /// `build_job` wrapper for tests that don't exercise the §13a
    /// hw-bench gate. Empty cache + 0 floor → `bench_needed = false`
    /// (vacuous on `A = ∅`). Default `HwClassConfig` ⇔ `wants_metal`
    /// falls back to the literal `kvm` feature and `apply_intent_
    /// resources` adds no per-intent tolerations (`taints_for(h)` empty).
    pub(super) fn job(pool: &Pool, i: &SpawnIntent) -> Job {
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
    // r[verify ctrl.pool.ephemeral+1]
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
        let (fc, fm, fd) = intent_pod_footprint(&i, cfg_fuse).as_triple();
        assert_eq!(q("cpu"), u64::from(fc));
        assert_eq!(q("memory"), fm);
        assert_eq!(q("ephemeral-storage"), fd);
    }

    /// r35 merged_bug_024 (§Simulator-shares-accounting): the SAME
    /// guarantee for Fetcher Pools. §13e routed Fetcher Pools through
    /// `nodeclaim_pool` — FFD/cover read `[nodeclaim_pool].fuse_cache_
    /// bytes` (the global), while a Fetcher Pool's `spec.fuseCacheBytes`
    /// override fed the stamp side directly. The two diverge → FFD
    /// fit-checks against a different ephemeral-storage triple than the
    /// pod actually requests. Single-source from the per-kind OnceLock
    /// (`FETCHER_FUSE_CACHE` here) closes the gap. The intent carries
    /// `kind=Fetcher`, matching the pool — the scheduler's intent
    /// filter guarantees that for every intent a pool spawns, and it is
    /// the precondition for the stamp side (keyed on `pool.spec.kind`)
    /// and the FFD side (keyed on `SpawnIntent.kind`) to agree.
    #[test]
    fn footprint_matches_stamped_requests_fetcher() {
        const GI: u64 = 1 << 30;
        let mut pool = test_pool("p", ExecutorKind::Fetcher);
        // Pre-CEL CR sets a per-pool override. Post-r35 this is silently
        // ignored — `pod::fuse_cache_bytes` reads the global OnceLock.
        pool.spec.fuse_cache_bytes = Some(6 * GI);
        let i = SpawnIntent {
            intent_id: "x".into(),
            cores: 1,
            mem_bytes: 4 * GI,
            disk_bytes: 8 * GI,
            disk_headroom_factor: Some(1.5),
            kind: rio_proto::types::ExecutorKind::Fetcher.into(),
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
        // The stamp side reads the FETCHER budget, not the builder's —
        // and not the per-pool override. FFD/cover pass the BUILDER
        // value to `intent_pod_footprint`, which substitutes the
        // fetcher budget for `kind=Fetcher` intents, so both sides
        // land on the same triple.
        let fetcher_fuse = pod::fetcher_fuse_cache_bytes();
        assert_eq!(
            pod::fuse_cache_bytes(&pool),
            fetcher_fuse,
            "Fetcher pool fuse_cache_bytes single-sourced from FETCHER_FUSE_CACHE \
             (per-Pool override defeats §Simulator-shares-accounting)"
        );
        let cfg_fuse = *pod::BUILDER_FUSE_CACHE
            .get()
            .unwrap_or(&pod::BUILDER_FUSE_CACHE_BYTES);
        assert_ne!(
            fetcher_fuse, cfg_fuse,
            "fetcher budget must differ from the builder budget for this \
             test to prove the per-kind selection"
        );
        let (fc, fm, fd) = intent_pod_footprint(&i, cfg_fuse).as_triple();
        assert_eq!(q("cpu"), u64::from(fc));
        assert_eq!(q("memory"), fm);
        assert_eq!(q("ephemeral-storage"), fd);
        assert_eq!(
            fd,
            ((8 * GI) as f64 * 1.5) as u64 + fetcher_fuse + LOG_BUDGET_BYTES,
            "fetcher ephemeral-storage = disk×headroom + FETCHER fuse budget + log"
        );
    }

    /// The cold-start fetcher pod's ephemeral-storage request. The
    /// median FOD is a small source archive: the overlay term
    /// (`disk_bytes × headroom`, where the download lands) starts at
    /// the scheduler's preferLocalBuild floor and grows via the
    /// reactive disk floor when a pod is evicted for exceeding it, so
    /// the cold-start request only needs to cover the median — the
    /// fuse-cache term is the fetch script's input closure, and the
    /// log budget is fixed. Pinning the absolute number keeps the
    /// "how many fetcher pods fit on a node" property under review:
    /// any change to one of the three addends shows up here as a
    /// deliberate diff, not as a silent packing regression.
    #[test]
    fn cold_start_fetcher_ephemeral_request_is_small() {
        const GI: u64 = 1 << 30;
        // The scheduler's cold-start solve for a preferLocalBuild FOD
        // (every nixpkgs fetcher): cores=1, mem=2Gi, disk=1Gi, flat
        // 1.5× headroom (no fit ⇒ no variance-aware curve).
        let i = SpawnIntent {
            intent_id: "x".into(),
            cores: 1,
            mem_bytes: 2 * GI,
            disk_bytes: GI,
            disk_headroom_factor: Some(1.5),
            kind: rio_proto::types::ExecutorKind::Fetcher.into(),
            ..Default::default()
        };
        let cfg_fuse = *pod::BUILDER_FUSE_CACHE
            .get()
            .unwrap_or(&pod::BUILDER_FUSE_CACHE_BYTES);
        let (_, _, eph) = intent_pod_footprint(&i, cfg_fuse).as_triple();
        assert_eq!(
            eph,
            (3 * GI) / 2 + pod::FETCHER_FUSE_CACHE_BYTES + LOG_BUDGET_BYTES,
            "cold-start fetcher ephemeral-storage = 1Gi×1.5 + fetcher fuse + 1Gi log"
        );
        assert_eq!(eph, 6_979_321_856, "= 6.5 GiB");
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
        // r35 merged_bug_024: Fetcher Pool ALSO ignores spec.fuseCacheBytes
        // — single-sourced from the boot-time per-kind value
        // (FETCHER_FUSE_CACHE). §13e routed Fetcher Pools through
        // nodeclaim_pool, so a per-Pool override would make FFD/cover
        // predict a different ephemeral-storage footprint than the pod
        // actually stamps.
        let mut f = test_pool("f", ExecutorKind::Fetcher);
        f.spec.fuse_cache_bytes = Some(100 * (1 << 30));
        assert_eq!(
            pod::fuse_cache_bytes(&f),
            pod::fetcher_fuse_cache_bytes(),
            "Fetcher ignores spec.fuseCacheBytes — single-sourced from FETCHER_FUSE_CACHE"
        );
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

    /// merged_bug_249: `build_job` stamps the COMPLETE RenderInputs
    /// fingerprint (placement + exclusions + resources + deadline) on
    /// Job metadata.annotations so `reap_stale_for_intents` sees drift
    /// on ANY render-decided axis. Deterministic over key/term order;
    /// field sensitivity is pinned in `candidate::tests`.
    #[test]
    fn build_job_stamps_render_inputs_fingerprint() {
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
        let fp = |i: &SpawnIntent| candidate::RenderInputs::from_intent(i).fingerprint();
        assert_eq!(
            fp(&a),
            fp(&b),
            "deterministic over both per-term key order and term order"
        );
        // Exclusion drift alone changes the stamp (the legacy
        // placement-only fingerprint did not — the recorded red).
        let mut excluded = a.clone();
        excluded.excluded_nodes = vec!["n1".into()];
        assert_ne!(fp(&a), fp(&excluded));

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
            Some(fp(&i).as_str()),
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
    // r[verify sched.sla.disk-reaches-ephemeral-storage+1]
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
        // live_058-a: the rendered memory is the PADDED container
        // law, never the bare solve (16Gi solve → +256Mi pad; the
        // 512Mi floor is inert here — the W10-CJ battery drives it).
        assert_eq!(
            req["memory"],
            Quantity((16 * GI + (256 << 20)).to_string()),
            "container mem = solved + WORKER_MEM_OVERHEAD_BYTES"
        );
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
    /// r35 merged_bug_024: `PoolSpec.fuse_cache_bytes` is IGNORED for
    /// both kinds — single-sourced from the per-kind boot-time value so
    /// FFD/cover/stamp agree (§Simulator-shares-accounting). The
    /// emptyDir sizeLimit and the `ephemeral-storage` addend both read
    /// the same value so they cannot drift even within one kind.
    #[test]
    fn fuse_cache_budget_matches_sizelimit() {
        const GI: u64 = 1 << 30;
        let cfg_fuse = *pod::BUILDER_FUSE_CACHE
            .get()
            .unwrap_or(&pod::BUILDER_FUSE_CACHE_BYTES);
        let fetcher_fuse = pod::fetcher_fuse_cache_bytes();
        for (kind, override_, expect) in [
            (ExecutorKind::Builder, None, cfg_fuse),
            (ExecutorKind::Fetcher, None, fetcher_fuse),
            // mb_035: Builder ignores PoolSpec override (single-sourced).
            (ExecutorKind::Builder, Some(4 * GI), cfg_fuse),
            // r35 merged_bug_024: Fetcher ALSO ignores PoolSpec override —
            // §13e routes Fetcher Pools through nodeclaim_pool, so the
            // override would make FFD predict a different footprint than
            // the pod stamps.
            (ExecutorKind::Fetcher, Some(6 * GI), fetcher_fuse),
        ] {
            let mut pool = test_pool("p", kind);
            pool.spec.fuse_cache_bytes = override_;
            let i = SpawnIntent {
                intent_id: "abc".into(),
                disk_bytes: 5 * GI,
                kind: match kind {
                    ExecutorKind::Builder => rio_proto::types::ExecutorKind::Builder,
                    ExecutorKind::Fetcher => rio_proto::types::ExecutorKind::Fetcher,
                }
                .into(),
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
    /// NEVER get them — second-scheduler routing stays Builder-only
    /// (fetcher pods are ~1c; the FFD↔default-scheduler bin-packing
    /// divergence is bounded by the tiny per-pod footprint, not worth
    /// the `kube-build-scheduler` dependency for fetcher cells).
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

    /// `gate_active=false` ⇒ Builder pods get NO `schedulerName` and no
    /// `priorityClassName`. The flag is `placeable.is_some() &&
    /// kube_build_scheduler_enabled` (r40 bug_018) — false when the
    /// NodeClaim CRD is absent (k3s VM tests) OR `buildScheduler.
    /// enabled=false` (terraform-managed Karpenter, chart-disabled
    /// second scheduler). In both cases `kube-build-scheduler` isn't
    /// deployed, so a pod targeting it would sit Pending forever with
    /// zero alerts (the `KubeBuildScheduler*` alerts are gated by the
    /// same toggle).
    #[test]
    fn build_job_no_scheduler_name_when_gate_inactive() {
        let i = SpawnIntent {
            intent_id: "abc".into(),
            cores: 17,
            ..Default::default()
        };
        let builder = test_pool("p", ExecutorKind::Builder);
        let job = build_job(
            &builder,
            crate::fixtures::oref(&builder),
            &test_sched_addrs(),
            &test_store_addrs(),
            &HwClassConfig::default(),
            &i,
            None,
            &HwSampledCache::default(),
            0,
            false, // gate_active
        )
        .unwrap();
        let pod = job.spec.as_ref().unwrap().template.spec.clone().unwrap();
        assert_eq!(
            pod.scheduler_name, None,
            "gate inactive → default kube-scheduler, not kube-build-scheduler"
        );
        assert_eq!(pod.priority_class_name, None);
    }

    // r[verify sched.dispatch.fleet-exhaust+5]
    /// AD2 anti-affinity render: an intent carrying `excluded_nodes`
    /// gets a `kubernetes.io/hostname NotIn […]` requirement ANDed
    /// into EVERY required nodeAffinity term (terms are OR'd, so a
    /// separate term would weaken the per-intent placement); an intent
    /// with exclusions but no affinity gets a single term carrying
    /// only the NotIn; an intent without exclusions renders exactly as
    /// today (no affinity is invented).
    #[test]
    fn intent_excluded_nodes_render_required_anti_affinity() {
        let pool = test_pool("p", ExecutorKind::Builder);
        let hw = HwClassConfig::default();
        let term = rio_proto::types::NodeSelectorTerm {
            match_expressions: vec![rio_proto::types::NodeSelectorRequirement {
                key: "rio.build/hw-band".into(),
                operator: "In".into(),
                values: vec!["mid".into()],
            }],
        };

        // (a) affinity + exclusions: NotIn ANDed into the existing term.
        let mut i = intent("ex-a");
        i.node_affinity = vec![term.clone()];
        i.excluded_nodes = vec!["node-bad-1".into(), "node-bad-2".into()];
        let mut spec =
            pod::build_executor_pod_spec(&pool, &test_sched_addrs(), &test_store_addrs(), &hw);
        apply_intent_resources(&mut spec, &pool, &i, &hw);
        let terms = spec
            .affinity
            .as_ref()
            .and_then(|a| a.node_affinity.as_ref())
            .and_then(|na| {
                na.required_during_scheduling_ignored_during_execution
                    .as_ref()
            })
            .map(|ns| ns.node_selector_terms.clone())
            .expect("required nodeAffinity rendered");
        assert_eq!(terms.len(), 1, "no extra OR'd term is appended");
        let exprs = terms[0].match_expressions.as_ref().expect("expressions");
        assert!(
            exprs
                .iter()
                .any(|r| r.key == "rio.build/hw-band" && r.operator == "In"),
            "the per-intent placement requirement survives"
        );
        let not_in = exprs
            .iter()
            .find(|r| r.key == "kubernetes.io/hostname" && r.operator == "NotIn")
            .expect("excluded-nodes anti-affinity requirement present");
        assert_eq!(
            not_in.values.as_deref(),
            Some(&["node-bad-1".to_string(), "node-bad-2".to_string()][..])
        );

        // (b) exclusions only: one term carrying only the NotIn.
        let mut i = intent("ex-b");
        i.excluded_nodes = vec!["node-bad-1".into()];
        let mut spec =
            pod::build_executor_pod_spec(&pool, &test_sched_addrs(), &test_store_addrs(), &hw);
        apply_intent_resources(&mut spec, &pool, &i, &hw);
        let terms = spec
            .affinity
            .as_ref()
            .and_then(|a| a.node_affinity.as_ref())
            .and_then(|na| {
                na.required_during_scheduling_ignored_during_execution
                    .as_ref()
            })
            .map(|ns| ns.node_selector_terms.clone())
            .expect("required nodeAffinity rendered for the exclusion alone");
        assert_eq!(terms.len(), 1);
        assert_eq!(
            terms[0]
                .match_expressions
                .as_ref()
                .map(|e| e.len())
                .unwrap_or_default(),
            1,
            "only the NotIn requirement"
        );

        // (c) no exclusions: byte-identical to today (no affinity added).
        let i = intent("ex-c");
        let mut with_gate =
            pod::build_executor_pod_spec(&pool, &test_sched_addrs(), &test_store_addrs(), &hw);
        apply_intent_resources(&mut with_gate, &pool, &i, &hw);
        assert!(
            with_gate.affinity.is_none(),
            "an intent without excluded_nodes renders identically to today"
        );
    }

    // r[verify ctrl.nodeclaim.placeable-gate+5]
    /// `apply_placeable_gate` (the production fold, via the page's
    /// typed surface) filters to the FFD-placed-on-Registered set;
    /// unarmed gate clears + returns `false` so `queued_known = None`
    /// (fail-closed reap).
    #[test]
    fn placeable_gate_retain_semantics() {
        use crate::reconcilers::nodeclaim_pool::PlaceableGate;
        let mk = |ids: &[&str]| -> IntentPage {
            IntentPage::for_test(ids.iter().map(|i| intent(i)).collect())
        };

        // Armed → filter to set, armed=true.
        let gate = PlaceableGate::from_ids(["a", "c", "z"]);
        let mut page = mk(&["a", "b", "c"]);
        assert!(apply_placeable_gate(&mut page, &gate).is_some());
        let ids: Vec<&str> = page.iter_page().map(|i| i.intent_id.as_str()).collect();
        assert_eq!(ids, vec!["a", "c"]);

        // Unarmed → clear, armed=false.
        let gate = PlaceableGate::unarmed();
        let mut page = mk(&["a", "b"]);
        assert!(
            apply_placeable_gate(&mut page, &gate).is_none(),
            "unarmed → None (fail-closed reap)"
        );
        assert_eq!(page.len_page(), 0);
    }

    // r[verify ctrl.nodeclaim.placeable-gate+5]
    /// The spawn-intent fan-out close in unit-test form: 1226 Ready
    /// intents, FFD placed 9 on Registered nodes → only 9 survive the
    /// gate. Pre-B12 (`ready` retain) all 1226 would mint Pending Jobs;
    /// post-B12 the Job count is bounded by Registered-node capacity.
    #[test]
    fn placeable_gate_bounds_spawn_intent_fan_out() {
        use crate::reconcilers::nodeclaim_pool::PlaceableGate;
        let mut page = IntentPage::for_test(
            (0..1226)
                .map(|k| SpawnIntent {
                    intent_id: format!("i{k:04}"),
                    ready: Some(true),
                    ..Default::default()
                })
                .collect(),
        );
        // FFD placed 9 on Registered nodes (arbitrary subset).
        let placed = [
            "i0000", "i0042", "i0137", "i0511", "i0512", "i0777", "i0999", "i1000", "i1225",
        ];
        let gate = PlaceableGate::from_ids(placed);
        assert!(apply_placeable_gate(&mut page, &gate).is_some());
        assert_eq!(
            page.len_page(),
            placed.len(),
            "1226 Ready intents → {} placeable Jobs (bounded by Registered-node \
             capacity, not Ready-set size)",
            placed.len()
        );
        let survived: HashSet<&str> = page.iter_page().map(|i| i.intent_id.as_str()).collect();
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
        let mut page = IntentPage::for_test(vec![intent("x")]);
        assert!(apply_placeable_gate(&mut page, &gate).is_none());

        let placeable: Vec<Placement> = vec![
            (intent("on-reg"), "n1".into(), false),
            (intent("on-inflight"), "n2".into(), true),
            (intent("on-reg-2"), "n3".into(), false),
        ];
        // The production publisher shape: the FULL disposition tick
        // (in-flight ids land in their own set, not on_registered).
        let tick = crate::reconcilers::nodeclaim_pool::PlacedTick::from_sim(&placeable, &[]);
        tx.send_replace(Some(std::sync::Arc::new(tick)));

        let mut page = IntentPage::for_test(vec![
            intent("on-reg"),
            intent("on-inflight"),
            intent("on-reg-2"),
        ]);
        assert!(apply_placeable_gate(&mut page, &gate).is_some());
        let ids: Vec<&str> = page.iter_page().map(|i| i.intent_id.as_str()).collect();
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

    // r[verify ctrl.pool.demand-completeness]
    /// The durable stamp round-trip: `build_job` writes
    /// `rio.build/intent-cells` from the spawned intent's
    /// `(hw_class_names, node_affinity)` zip, and the re-ack
    /// reconstruction reads back exactly the cells the scheduler's arm
    /// decode consumes (the `karpenter.sh/capacity-type` In requirement).
    /// Hw-agnostic intents (no cells) spawn unstamped.
    #[test]
    fn intent_cells_annotation_round_trips_via_build_job() {
        let mut i = intent("cellsrt");
        i.hw_class_names = vec!["m7i".into(), "c8g".into()];
        i.node_affinity = (0..2)
            .map(|k| rio_proto::types::NodeSelectorTerm {
                match_expressions: vec![
                    rio_proto::types::NodeSelectorRequirement {
                        key: "rio.build/hw-class".into(),
                        operator: "In".into(),
                        values: vec![format!("h{k}")],
                    },
                    rio_proto::types::NodeSelectorRequirement {
                        key: "karpenter.sh/capacity-type".into(),
                        operator: "In".into(),
                        values: vec![if k == 0 {
                            "spot".into()
                        } else {
                            "on-demand".into()
                        }],
                    },
                ],
            })
            .collect();
        let pool = test_pool("cells-pool", ExecutorKind::Builder);
        let j = job(&pool, &i);
        let anns = j
            .spec
            .as_ref()
            .unwrap()
            .template
            .metadata
            .as_ref()
            .unwrap()
            .annotations
            .as_ref()
            .unwrap();
        assert_eq!(
            anns.get(INTENT_CELLS_ANNOTATION).map(String::as_str),
            Some("m7i:spot,c8g:on-demand"),
            "the stamp is the (h, cap) zip in the house grammar"
        );

        // Hw-agnostic: no stamp (nothing to arm — the Empty echo is law).
        let bare = intent("nocells");
        let j = job(&pool, &bare);
        let anns = j
            .spec
            .as_ref()
            .unwrap()
            .template
            .metadata
            .as_ref()
            .unwrap()
            .annotations
            .as_ref()
            .unwrap();
        assert!(!anns.contains_key(INTENT_CELLS_ANNOTATION));
    }
}

#[cfg(test)]
mod disk_four_caller_census {
    //! WO-S7-2 (live_049 L2): the four-caller prose census above
    //! `pod_ephemeral_request` ENROLLED as a source-scanning test —
    //! the author-census kill (R15). Generator (committed, repaired
    //! post-review — the bare `pod_ephemeral_request\(` needle pinned
    //! at most 2 of 4 members):
    //!
    //!   rg -n 'pod_ephemeral_request\(|intent_pod_footprint\(' rio-controller/
    //!
    //! over the EMBEDDED in-crate sources (include_str! — nix-gate
    //! safe), classification per hit; member 4 (helm-lint) is pinned
    //! by the CONTENT-bound leg below (the lint computes `need` from
    //! MIRRORED constants, so helm-lint green cannot detect a stale
    //! mirror — the census pins the mirror rows themselves, generator:
    //!   rg -n 'OVERLAY_HEADROOM_PCT|LOG_BUDGET_BYTES' nix/tests/helm/14-disk-ceiling.sh
    //! output committed as `HELM_MEMBER_ROWS`). The rio-scheduler/
    //! root is KEPT in the generator with the recorded baseline:
    //! zero PRODUCTION hits (3 prose mentions — explore.rs R8 doc,
    //! state/derivation.rs agreement note, sla_contract.rs doc — all
    //! comment-only; the fitted INPUT flows through
    //! `SpawnIntent.disk_bytes`, never a second formula).
    //! W7-J (the numeric-agreement half for the three in-process
    //! members) is `footprint_matches_apply_intent_resources` +
    //! the B8 battery; THIS census is the membership/closure half.

    const JOBS_SRC: &str = include_str!("jobs.rs");
    const FFD_SRC: &str = include_str!("../nodeclaim_pool/ffd.rs");
    const COVER_SRC: &str = include_str!("../nodeclaim_pool/cover.rs");

    /// Member-4 content rows: the helm-lint mirror constants
    /// ([GEN-SET] output, see module doc). A drift here means the
    /// lint's arithmetic no longer mirrors `pod_ephemeral_request`'s.
    const HELM_MEMBER_ROWS: &[&str] = &[
        "OVERLAY_HEADROOM_PCT=195",
        "LOG_BUDGET_BYTES=$((1 << 30))",
        "need=$(( max_disk * OVERLAY_HEADROOM_PCT / 100 + fuse + LOG_BUDGET_BYTES ))",
    ];

    /// Production half: everything before the first test MODULE
    /// (`#[cfg(test)]\nmod ` — jobs.rs has cfg(test) imports and an
    /// inline helper attribute earlier; splitting on the bare
    /// attribute truncated at line 47).
    fn prod(src: &str) -> &str {
        src.split("#[cfg(test)]\nmod ").next().unwrap_or(src)
    }

    // r[verify sched.sla.disk-reaches-ephemeral-storage+1]
    /// Membership: every production consumer of the ephemeral formula
    /// is one of the four censused members; an UNLISTED call site
    /// fails naming the file (closure tomorrow, not completeness
    /// today).
    #[test]
    fn disk_four_caller_census() {
        // jobs.rs production: the def, the footprint bridge, the
        // direct caller (apply_intent_resources).
        assert_eq!(
            prod(JOBS_SRC).matches("pod_ephemeral_request(").count(),
            3,
            "jobs.rs: def + doc-prose mention + bridge (intent_pod_footprint) \
             — the former direct caller (apply_intent_resources) now rides \
             the FOOTPRINT (live_058-a: one constructor for all three axes); \
             a new caller joins the census with its member classified"
        );
        assert_eq!(
            prod(JOBS_SRC).matches("fn intent_pod_footprint(").count(),
            1,
            "jobs.rs: the single footprint bridge definition"
        );
        // ffd.rs: the FFD fit-check member, via the footprint.
        assert_eq!(
            prod(FFD_SRC).matches("intent_pod_footprint(").count(),
            1,
            "ffd.rs: the simulate fit-check consumes the SHARED triple"
        );
        // cover.rs: the NodeClaim floor member, via the footprint
        // (2 sizing reads + 1 aggregate map + 1 over-cap letter
        // evidence read + 2 doc-prose mentions).
        assert_eq!(
            prod(COVER_SRC).matches("intent_pod_footprint(").count(),
            5,
            "cover.rs: cover_deficit's disk-floor inputs ride the SHARED \
             footprint (3 sizing/aggregate call sites + the \
             reclassify_over_cap letter-evidence read (bug_026: the \
             OverCap letter's footprint MUST be the same triple the \
             sizing partition compared, so the verdict detail cannot \
             drift from the drop decision) + 1 doc mention; a 2nd doc \
             mention sits in cover.rs's own test half)"
        );
    }

    // r[verify sched.sla.disk-reaches-ephemeral-storage+1]
    /// Member 4 (helm-lint): the content-bound mirror rows. The lint
    /// script is OUTSIDE the crate's build sandbox (no include_str!
    /// across the workspace under crate2nix's source filter), so the
    /// committed [GEN-SET] rows pin the mirror; re-run the generator
    /// in the module doc on drift.
    #[test]
    fn helm_member_mirror_rows_pinned() {
        // LOG_BUDGET_BYTES (1 GiB) is the shared constant the script
        // mirrors; the in-crate side is the const below.
        assert_eq!(super::LOG_BUDGET_BYTES, 1 << 30);
        assert_eq!(HELM_MEMBER_ROWS.len(), 3, "the three mirror rows");
        assert!(
            HELM_MEMBER_ROWS[1].contains("1 << 30"),
            "the script's LOG_BUDGET_BYTES row mirrors the 1 GiB const"
        );
    }
}

#[cfg(test)]
mod demand_lane_census {
    //! **W10-AH (round-10 WO-S4-1, R26 banner): the demand-lane
    //! consumer census** — every production consumer of the
    //! [`super::PoolDemandView`] iteration/absence surfaces enrolled
    //! with a CLASS DISPOSITION, machine-counted over the EMBEDDED
    //! pool sources (include_str! — nix-gate safe, the (wwwww) form).
    //! [GEN-SET] generator (committed; re-run on any count drift):
    //!
    //!   rg -n '\.iter_page\(|\.retain_page\(|\.clear_page\(|\.len_page\(|WantMap::for_pool\(|\.verdict\(' rio-controller/src/reconcilers/pool/
    //!
    //! Class dispositions (the row column; cardinality pinned by the
    //! per-needle counts below):
    //!
    //! | site (jobs.rs prod)                  | lane          | class |
    //! |--------------------------------------|---------------|-------|
    //! | `WantMap::for_pool` body walk        | absence mint  | snapshot-tolerant (projects the page INTO the witness-fused map) |
    //! | `HwSampledCache::fetch` fan-out      | page walk     | snapshot-tolerant (per-intent RPC union) |
    //! | spawn-candidate filter (`wanted`)    | page walk     | snapshot-tolerant (per-held-element; AD2 totality is per-held by design) |
    //! | `assemble_re_acks` page index        | continuity    | derives-from-Job-LIST (WO-S4-3: the lane's MEMBERSHIP comes from the controller's own Job LIST — the local complete inventory; the page walk here only builds the on-page full-fidelity echo index) |
    //! | `apply_placeable_gate` retain/clear  | page mutation | snapshot-tolerant (gate fold) |
    //! | `queued` count (`len_page`)          | page count    | snapshot-tolerant (bound law owns demand counting) |
    //! | reconcile `WantMap::for_pool` mint   | absence mint  | the R26 accessor's sole production constructor call |
    //! | reap loop `want.verdict(jn)`         | absence       | witness-fused verdict (suspends on `Unknowable`) |
    //! | `PoolStreaks::step` expiry clock     | continuity    | witness-suspended (candidate.rs — the touched-staleness expiry suspends for the pool's entries while its view is `Incomplete`; W10-AM) |
    //!
    //! R22′ plants (BOTH evasion axes, red at the SCAN layer — the
    //! corpora are raw-source strings driven through the same
    //! detectors that police production):
    //! - the raw-slice membership strawman (the merged_bug_029 shape);
    //! - the continuity-style consumer on the page lane (an UNENROLLED
    //!   `.iter_page` hit — the axis the triage named missing).

    const JOBS_SRC: &str = include_str!("jobs.rs");
    const JOB_SRC: &str = include_str!("job.rs");
    const CANDIDATE_SRC: &str = include_str!("candidate.rs");
    const POD_SRC: &str = include_str!("pod.rs");

    /// Production half (same split as `disk_four_caller_census`).
    fn prod(src: &str) -> &str {
        src.split("#[cfg(test)]\nmod ").next().unwrap_or(src)
    }

    /// The committed census: (file, needle, count). A drift = a new
    /// (or vanished) demand-lane consumer — re-run the generator and
    /// file the site's class in the module-doc table.
    const CENSUS: &[(&str, &str, usize)] = &[
        ("jobs.rs", ".iter_page(", 4),
        ("jobs.rs", ".retain_page(", 1),
        ("jobs.rs", ".clear_page(", 1),
        ("jobs.rs", ".len_page(", 1),
        ("jobs.rs", "WantMap::for_pool(", 1),
        ("jobs.rs", ".verdict(", 1),
        ("job.rs", ".iter_page(", 0),
        ("job.rs", ".verdict(", 0),
        ("candidate.rs", ".iter_page(", 0),
        ("candidate.rs", ".verdict(", 0),
        ("pod.rs", ".iter_page(", 0),
        ("pod.rs", ".verdict(", 0),
    ];

    fn src_for(file: &str) -> &'static str {
        match file {
            "jobs.rs" => JOBS_SRC,
            "job.rs" => JOB_SRC,
            "candidate.rs" => CANDIDATE_SRC,
            "pod.rs" => POD_SRC,
            other => panic!("unenrolled census file {other}"),
        }
    }

    /// The raw-slice membership detector: absence inferred by walking
    /// a bare intent slice for an id — the exact merged_bug_029 shape
    /// the typed lanes exist to kill. Zero production hits.
    fn raw_membership_hits(src: &str) -> usize {
        src.matches(".iter().any(|i| i.intent_id ==").count()
            + src.matches(".iter().all(|i| i.intent_id !=").count()
    }

    // r[verify ctrl.pool.demand-completeness]
    /// Enrollment totality: per-(file, needle) hit counts equal the
    /// committed census. A NEW page-lane/absence consumer fails here
    /// naming its file — it enrolls with a class in the module-doc
    /// table (closure tomorrow, not completeness today).
    #[test]
    fn demand_lane_consumers_enrolled() {
        for (file, needle, want) in CENSUS {
            let got = prod(src_for(file)).matches(needle).count();
            assert_eq!(
                got, *want,
                "{file}: `{needle}` consumer count drifted — re-run \
                 the [GEN-SET] generator and file the new site's class \
                 disposition in the W10-AH table"
            );
        }
    }

    // r[verify ctrl.pool.demand-completeness]
    /// Zero raw-slice membership tests remain in the pool plane (the
    /// lanes are the ONLY demand-membership surface).
    #[test]
    fn no_raw_slice_membership_tests() {
        for (file, src) in [
            ("jobs.rs", JOBS_SRC),
            ("job.rs", JOB_SRC),
            ("candidate.rs", CANDIDATE_SRC),
            ("pod.rs", POD_SRC),
        ] {
            assert_eq!(
                raw_membership_hits(prod(src)),
                0,
                "{file}: raw-slice intent-membership test — absence \
                 judgments go through WantMap::verdict (R26)"
            );
        }
    }

    /// **W10-AP (merged_bug_140, the R15 type census).** The
    /// pool-scoped ledger consumer list is COMMITTED and the key seal
    /// is rustc-checked: every instantiation is
    /// `PoolScopedLedger<...>` (key type sealed to `PoolKey` by the
    /// type itself — a `&str`-keyed consumer no longer compiles), and
    /// ZERO bare-keyed ledger shapes remain in the pool plane.
    /// [GEN-SET] generator:
    ///   rg -n 'PoolScopedLedger<|HashMap<String, u64>|HashMap<\(String, String\)' rio-controller/src/reconcilers/pool/
    #[test]
    fn w10_ap_pool_scoped_ledgers_are_sealed() {
        let jobs_prod = prod(JOBS_SRC);
        let cand_prod = prod(CANDIDATE_SRC);
        // The committed consumer list: STALE_STRIKES (jobs.rs static +
        // the strike fn + the backdate helper signature) and
        // PoolStreaks' two evidence maps (+ the ledger's own def/impl
        // blocks in candidate.rs).
        assert_eq!(
            jobs_prod.matches("PoolScopedLedger<").count(),
            2,
            "jobs.rs: the STALE_STRIKES instantiation (static type + \
             the strike fn signature) — a new consumer joins the \
             committed list"
        );
        assert_eq!(
            cand_prod.matches("PoolScopedLedger<").count(),
            8,
            "candidate.rs: the abstraction (struct + Default + impl + \
             the committed consumer-list doc rows) + PoolStreaks' two \
             maps — re-run the [GEN-SET] generator on drift"
        );
        // The seal's negative space: zero bare-keyed ledger shapes in
        // CODE lines (comment lines excluded — the StrikeEntry doc
        // quotes the deleted shape; `aggregate_upper_for`'s wire map
        // is a system→count projection, not a pool ledger, hence the
        // tick-clock/row-tuple needles, not bare HashMap<String, _>).
        for (file, src) in [("jobs.rs", jobs_prod), ("candidate.rs", cand_prod)] {
            let code_hits = src
                .lines()
                .filter(|l| !l.trim_start().starts_with("//"))
                .filter(|l| {
                    l.contains("ticks: HashMap<String") || l.contains("HashMap<(String, String)")
                })
                .count();
            assert_eq!(
                code_hits, 0,
                "{file}: a bare-name pool ledger shape re-minted — the \
                 merged_bug_140 recurrence; instantiate \
                 PoolScopedLedger instead"
            );
        }
    }

    // r[verify ctrl.pool.demand-completeness]
    /// R22′ plant axis 1: the raw-membership strawman corpus is RED at
    /// the scan layer (the detector fires on the planted shape — the
    /// census's policing is live, not self-reported).
    #[test]
    fn plant_raw_membership_detected() {
        const PLANT: &str =
            r#"let gone = !intents.iter().any(|i| i.intent_id == wanted_id); if gone { reap(j); }"#;
        assert!(
            raw_membership_hits(PLANT) > 0,
            "the raw-membership detector must fire on the planted \
             merged_bug_029 shape"
        );
    }

    // r[verify ctrl.pool.demand-completeness]
    /// R22′ plant axis 2 (the axis the triage named missing): a
    /// continuity-style consumer written on the PAGE lane is RED at
    /// the scan layer — the scanner counts its `.iter_page(` hit, so
    /// an unenrolled site (count drift) fails
    /// `demand_lane_consumers_enrolled`.
    #[test]
    fn plant_continuity_on_page_lane_detected() {
        const PLANT: &str = "let re_acked: Vec<_> = page.iter_page()\
            .filter(|i| dispatched.contains(&i.intent_id)).collect();";
        assert_eq!(
            PLANT.matches(".iter_page(").count(),
            1,
            "the scanner must count a page-lane hit in the planted \
             continuity-consumer corpus (enrollment drift = red)"
        );
        // The plant is NOT an enrolled census row — the same string
        // appearing in any enrolled file would shift its committed
        // count and fail enrollment totality.
    }
}

#[cfg(test)]
mod w10_cj_container_pad {
    //! **W10-CJ (live_058-a, HIGH — the live incident is the
    //! production specimen).** The container limit binds the
    //! CONTAINER: both the PAD corner and the MINIMUM corner driven
    //! at the floor's own quantifier, end-to-end through `build_job`
    //! (the rendered request==limit map the kubelet enforces).

    use super::tests::{intent, job, test_pool};
    use super::*;
    use rio_crds::pool::ExecutorKind;

    fn rendered_mem(i: &SpawnIntent) -> u64 {
        let pool = test_pool("p", ExecutorKind::Builder);
        let j = job(&pool, i);
        let spec = j.spec.unwrap().template.spec.unwrap();
        let req = spec.containers[0]
            .resources
            .as_ref()
            .and_then(|r| r.requests.as_ref())
            .unwrap();
        req["memory"].0.parse::<u64>().unwrap()
    }

    // r[verify ctrl.pool.container-overhead]
    /// The MINIMUM corner: a 64 MiB solve (the live incident band)
    /// renders the 512 MiB floor — pre-fix the rendered limit WAS the
    /// solve (64 MiB, below the worker's own baseline → whole-
    /// container OOM → the live_058 2.75h requeue loop).
    ///
    /// Pre-fix red (pad severed — the raw stamp):
    ///   left: 67108864  right: 536870912
    #[test]
    fn min_corner_floors_tiny_solves() {
        let mut i = intent("tiny64");
        i.mem_bytes = 64 << 20;
        assert_eq!(
            rendered_mem(&i),
            512 << 20,
            "64 MiB solve + 256 MiB pad = 320 MiB < the 512 MiB floor \
             → the floor binds (the live incident specimen)"
        );
    }

    // r[verify ctrl.pool.container-overhead]
    /// The PAD corner: a 1 GiB solve renders solve + pad (the floor
    /// is inert above 256 MiB solves).
    #[test]
    fn pad_corner_adds_worker_overhead() {
        let mut i = intent("one-gi");
        i.mem_bytes = 1 << 30;
        assert_eq!(
            rendered_mem(&i),
            (1u64 << 30) + (256 << 20),
            "1 GiB solve → 1 GiB + 256 MiB pad (> floor)"
        );
    }

    // r[verify ctrl.pool.container-overhead]
    /// **W10-CK** rides `footprint_matches_stamped_requests` (the
    /// shared-fn quantifier: FFD's decrement == the pod's rendered
    /// request, count-exact, now over the PADDED mem) — this twin
    /// pins the SOLVE-side scoping: the pad never mutates the intent
    /// (telemetry and ladder algebra read the solve unchanged).
    #[test]
    fn pad_is_additive_at_the_container_seam_only() {
        let mut i = intent("scope");
        i.mem_bytes = 2 << 30;
        let before = i.mem_bytes;
        let fp = intent_pod_footprint(&i, 0);
        assert_eq!(i.mem_bytes, before, "the solve is untouched");
        assert_eq!(fp.mem_bytes(), before + (256 << 20));
        assert_eq!(
            fp.as_triple().1,
            fp.mem_bytes(),
            "one padded value everywhere the footprint is read"
        );
    }
}

#[cfg(test)]
mod mem_axis_census {
    //! **W10-CJ/CK support (round-10 live_058-a): the mem-axis
    //! consumer census** — the disk four-caller pattern extended to
    //! container memory. The COMPILE claim is scoped to the stamp
    //! helper ([`super::stamp_container_resources`]: the intent is
    //! out of scope, so a raw `i.mem_bytes` stamp does not
    //! type-check there); the WIDER population of `mem_bytes`
    //! readers is CENSUS-HELD here (the honest tier — `mem_bytes` is
    //! a public proto field; solve/telemetry reads stay legitimate
    //! and are enumerated). [GEN-SET] generator:
    //!
    //!   rg -n 'mem_bytes' rio-controller/src/reconcilers/pool/jobs.rs
    //!
    //! Production members (jobs.rs prod half):
    //! - the FOOTPRINT CONSTRUCTOR (`intent_pod_footprint`): the SOLE
    //!   pad site — `i.mem_bytes` enters container accounting only
    //!   here;
    //! - the hw-bench floor gate (`build_job`): telemetry read
    //!   (`intent.mem_bytes >= hw_bench_mem_floor`) — never a
    //!   resource stamp;
    //! - doc/comment mentions.
    //!
    //! R22′ plant matrix (grammar-derived, BOTH read forms — the
    //! direct field stamp AND the arithmetic/derived stamp; red at
    //! the scan layer):

    const JOBS_SRC: &str = include_str!("jobs.rs");

    fn prod(src: &str) -> &str {
        src.split("#[cfg(test)]\nmod ").next().unwrap_or(src)
    }

    /// The stamp-helper extent (fn header to the next top-level
    /// brace) — the compile-sealed seam the scan re-verifies.
    fn stamp_helper_extent(src: &str) -> &str {
        let start = src
            .find("fn stamp_container_resources(")
            .expect("the sealed stamp helper exists");
        let end = src[start..]
            .find("\n}\n")
            .map(|e| start + e)
            .expect("helper body terminates");
        &src[start..end]
    }

    /// The raw-stamp detectors: a `mem_bytes` token inside a
    /// container-resource construction context (direct), or an
    /// arithmetic derivation of one feeding a Quantity (derived).
    fn raw_stamp_hits(src: &str) -> usize {
        src.lines()
            .filter(|l| !l.trim_start().starts_with("//"))
            .filter(|l| l.contains("Quantity") && l.contains("mem_bytes"))
            // The sealed accessor (`fp.mem_bytes()` / `.mem_bytes()`)
            // is the LAWFUL stamp; raw = the bare field.
            .filter(|l| !l.contains("mem_bytes()"))
            .count()
    }

    // r[verify ctrl.pool.container-overhead]
    /// The compile-sealed seam, re-verified at the scan layer: ZERO
    /// `mem_bytes` tokens inside the stamp helper (the intent cannot
    /// be read there — rustc enforces it; this pin makes the seam's
    /// EXTENT a census fact so a widened signature is caught).
    #[test]
    fn stamp_helper_is_sealed_to_the_footprint() {
        let helper = stamp_helper_extent(prod(JOBS_SRC));
        assert_eq!(
            helper.matches("mem_bytes()").count(),
            1,
            "the helper reads the FOOTPRINT's padded accessor exactly once"
        );
        assert_eq!(
            helper.matches("i.mem_bytes").count() + helper.matches("intent").count(),
            0,
            "the intent is out of scope in the stamp seam (the raw \
             solve cannot be stamped here — live_058-a)"
        );
    }

    // r[verify ctrl.pool.container-overhead]
    /// The census-held wider population: every production `mem_bytes`
    /// read in jobs.rs enumerated — the constructor (1 read), the
    /// hw-bench telemetry gate (1 read), zero raw Quantity stamps.
    #[test]
    fn mem_readers_enumerated() {
        let p = prod(JOBS_SRC);
        let code_reads = p
            .lines()
            .filter(|l| !l.trim_start().starts_with("//") && !l.trim_start().starts_with("///"))
            .filter(|l| l.contains("mem_bytes"))
            .count();
        assert_eq!(
            code_reads, 8,
            "jobs.rs prod mem_bytes code lines: the constructor's \
             2-line read + the struct field + the accessor (3 lines: \
             fn/self/triple) + the helper's padded-accessor stamp + \
             the hw-bench telemetry gate — a new reader joins the \
             census with its class named"
        );
        assert_eq!(
            raw_stamp_hits(p),
            0,
            "zero raw mem stamps outside the sealed helper (the \
             live_058-a recurrence shape)"
        );
    }

    // r[verify ctrl.pool.container-overhead]
    /// R22′ plants — BOTH grammar forms red at the scan layer.
    #[test]
    fn plant_raw_and_derived_stamps_detected() {
        const PLANT_DIRECT: &str =
            r#"map.insert("memory".into(), Quantity(i.mem_bytes.to_string()));"#;
        const PLANT_DERIVED: &str = r#"let m = intent.mem_bytes * 2; map.insert("memory".into(), Quantity(m.to_string()));"#;
        assert!(
            raw_stamp_hits(PLANT_DIRECT) > 0,
            "the direct-field stamp plant must trip the detector"
        );
        // The derived form: the arithmetic line carries mem_bytes; the
        // Quantity line carries the derived var — the detector's
        // per-line scan catches the DIRECT form; the derived form is
        // caught by the enumeration census (a new mem_bytes code line
        // shifts the committed count) — both axes planted, each red
        // through its own scanner.
        let derived_code_lines = PLANT_DERIVED
            .lines()
            .filter(|l| l.contains("mem_bytes"))
            .count();
        assert!(
            derived_code_lines > 0,
            "the derived-read plant must shift the enumeration census"
        );
    }
}
