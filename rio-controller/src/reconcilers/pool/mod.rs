//! Pool reconciler: spawn/reap one-shot rio-builder Jobs.
//!
//! Reconcile flow:
//! 1. Poll `GetSpawnIntents` for per-drv intents.
//! 2. Spawn Jobs up to `spec.maxConcurrent` (see `jobs.rs`).
//! 3. Reap excess Pending and orphan Running Jobs.
//! 4. Patch Pool.status from active Job count.
// r[impl ctrl.pool.reconcile]
// r[impl ctrl.crd.pool]
// r[impl ctrl.reconcile.owner-refs]
// r[impl ctrl.drain.sigterm]
//!
//! Server-side apply throughout: we PATCH with `fieldManager:
//! rio-controller`, K8s merges. Idempotent — same patch twice is
//! a no-op. No GET-modify-PUT race.
//!
//! Finalizer wraps everything: delete → cleanup() is a no-op (Jobs
//! carry an ownerReference to the Pool) → finalizer removed →
//! K8s ownerRef-GC cascades to the Jobs → their pods get SIGTERM and
//! drain at the executor level.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use k8s_openapi::api::core::v1::{PodSpec, Toleration};
use k8s_openapi::apimachinery::pkg::api::resource::Quantity;
use kube::ResourceExt;
use kube::runtime::controller::Action;
use tracing::info;

use crate::error::{Error, Result};
use crate::reconcilers::{Ctx, finalized, standard_error_policy, timed};
use rio_crds::pool::{ExecutorKind, Pool, SeccompProfileKind};

pub(super) mod conditions;
pub mod disruption;
pub(super) mod job;
pub(super) mod jobs;
pub mod pod;

use pod::{ExecutorPodParams, UpstreamAddrs};

#[cfg(test)]
pub(super) mod tests;

/// Finalizer name. Kubebuilder convention: `{kind}.{group}/{suffix}`
/// — the kind is the authoritative part (which controller owns this),
/// suffix describes WHAT the finalizer gates. K8s stores this in
/// `metadata.finalizers`; delete blocks until we remove it.
const FINALIZER: &str = "pool.rio.build/drain";

/// FUSE cache emptyDir sizeLimit for builder pods. Kubelet evicts on
/// overshoot. No CRD knob: pods are one-shot so the cache never
/// outlives one build's input closure.
pub(crate) const BUILDER_FUSE_CACHE: &str = "50Gi";

/// Default FUSE cache size for fetchers. FODs are typically small
/// (source tarballs, git clones) — 10Gi is plenty.
const FETCHER_FUSE_CACHE: &str = "10Gi";

/// Label every Pool-owned pod carries. `executor_labels()` sets it;
/// `disruption::run` filters on it; `jobs` + cleanup list-selectors
/// match on it.
pub(crate) use pod::POOL_LABEL;

/// Top-level reconcile. [`timed`] opens the `reconcile{reconciler,
/// name, ns}` span and records the duration histogram; `finalized`
/// handles the `metadata.finalizers` dance (Apply on normal
/// reconcile, Cleanup when deletionTimestamp is set).
pub async fn reconcile(pool: Arc<Pool>, ctx: Arc<Ctx>) -> Result<Action> {
    timed("pool", pool, ctx, |pool, ctx| {
        finalized(pool, ctx, FINALIZER, apply, cleanup)
    })
    .await
}

/// K8s Event reason for hostNetwork + !privileged spec-degrade.
/// Referenced by disruption_tests.rs event-reason reachability tests.
pub(crate) const REASON_HOST_USERS_SUPPRESSED: &str = "HostUsersSuppressedForHostNetwork";

/// Emit Warning events for every spec field the builder will silently
/// degrade. Each check mirrors a CEL rule at apply-time; the builder's
/// defensive override handles pre-CEL specs that the apiserver already
/// accepted, but the OPERATOR doesn't know their spec is stale unless
/// we surface it. Best-effort: event-publish failures are logged in
/// [`Ctx::publish_event`], never block reconcile.
// r[impl ctrl.event.spec-degrade]
async fn warn_on_spec_degrades(pool: &Pool, ctx: &Ctx) {
    use kube::runtime::events::{Event as KubeEvent, EventType};

    // r[impl ctrl.crd.host-users-network-exclusive]
    if pool.spec.host_network == Some(true) && pool.spec.privileged != Some(true) {
        ctx.publish_event(
            pool,
            &KubeEvent {
                type_: EventType::Warning,
                reason: REASON_HOST_USERS_SUPPRESSED.into(),
                note: Some(
                    "hostNetwork:true forces hostUsers omitted \
                     (K8s admission rejects the combo). Set \
                     privileged:true explicitly, or drop hostNetwork."
                        .into(),
                ),
                action: "Reconcile".into(),
                secondary: None,
            },
        )
        .await;
    }
}

/// Normal reconcile: make the world match spec.
async fn apply(pool: Arc<Pool>, ctx: Arc<Ctx>) -> Result<Action> {
    warn_on_spec_degrades(&pool, &ctx).await;
    jobs::reconcile(&pool, &ctx).await
}

/// Cleanup on delete. Jobs are one-shot and complete on their own;
/// in-flight builds finish naturally. Returning here removes the
/// finalizer and ownerReference GC deletes the Jobs.
async fn cleanup(pool: Arc<Pool>, _ctx: Arc<Ctx>) -> Result<Action> {
    info!(pool = %pool.name_any(), "cleanup: ownerRef GC handles Jobs");
    Ok(Action::await_change())
}

/// Requeue policy on error. Transient (Kube, Scheduler) →
/// exponential backoff (5s → 300s). InvalidSpec → fixed 5min.
pub fn error_policy(pool: Arc<Pool>, err: &Error, ctx: Arc<Ctx>) -> Action {
    standard_error_policy("pool", pool, err, ctx, Duration::from_secs(300))
}

/// Convert `Pool` → `ExecutorPodParams`, branching on `spec.kind`.
///
/// `ExecutorKind::Builder`: spec fields pass through verbatim; the
/// builder-only tuning knobs (daemon_timeout, fuse_passthrough)
/// become `extra_env` entries.
///
/// `ExecutorKind::Fetcher`: ADR-019 hardening — forced
/// `read_only_root_fs: true`, `seccomp = Localhost
/// operator/rio-fetcher.json`, `host_users` default `false` (spec
/// override honored — k3s escape hatch), default
/// `node-role: fetcher` selector + toleration, `tgps = 600`. CEL on
/// the CRD rejects fetcher specs that try to set the overridden
/// fields at admission time; this is belt-and-suspenders for pre-
/// CEL specs the apiserver already accepted.
pub(super) fn executor_params(pool: &Pool) -> ExecutorPodParams {
    match pool.spec.kind {
        ExecutorKind::Builder => {
            let mut extra_env = vec![];
            if let Some(p) = pool.spec.fuse_passthrough {
                extra_env.push(pod::env(
                    "RIO_FUSE_PASSTHROUGH",
                    if p { "true" } else { "false" },
                ));
            }
            if let Some(s) = pool.spec.daemon_timeout_secs {
                extra_env.push(pod::env("RIO_DAEMON_TIMEOUT_SECS", &s.to_string()));
            }
            ExecutorPodParams {
                role: ExecutorKind::Builder,
                read_only_root_fs: false,
                extra_env,
                pool_name: pool.name_any(),
                image: pool.spec.image.clone(),
                systems: pool.spec.systems.clone(),
                node_selector: pool.spec.node_selector.clone(),
                tolerations: pool.spec.tolerations.clone(),
                host_users: pool.spec.host_users,
                image_pull_policy: pool.spec.image_pull_policy.clone(),
                features: pool.spec.features.clone(),
                fuse_cache_quantity: Quantity(BUILDER_FUSE_CACHE.into()),
                fuse_threads: pool.spec.fuse_threads,
                privileged: pool.spec.privileged == Some(true),
                seccomp_profile: pool.spec.seccomp_profile.clone(),
                host_network: pool.spec.host_network,
                termination_grace_period_seconds: pool.spec.termination_grace_period_seconds,
            }
        }
        // r[impl ctrl.pool.fetcher-hardening]
        // r[impl fetcher.node.dedicated]
        // r[impl fetcher.sandbox.strict-seccomp]
        ExecutorKind::Fetcher => {
            // ADR-019 §Node isolation: fetchers land on dedicated
            // nodes via the `rio.build/fetcher=true:NoSchedule` taint
            // + matching selector. If the operator supplies their
            // own, honor those instead — lets them override for dev
            // clusters without dedicated node pools.
            let node_selector = pool.spec.node_selector.clone().or_else(|| {
                Some(BTreeMap::from([(
                    "rio.build/node-role".into(),
                    "fetcher".into(),
                )]))
            });
            let tolerations = pool.spec.tolerations.clone().or_else(|| {
                Some(vec![Toleration {
                    key: Some("rio.build/fetcher".into()),
                    operator: Some("Exists".into()),
                    effect: Some("NoSchedule".into()),
                    ..Default::default()
                }])
            });
            ExecutorPodParams {
                role: ExecutorKind::Fetcher,
                // ADR-019 §Sandbox hardening: rootfs tampering blocked.
                read_only_root_fs: true,
                extra_env: vec![],
                pool_name: pool.name_any(),
                image: pool.spec.image.clone(),
                systems: pool.spec.systems.clone(),
                node_selector,
                tolerations,
                // Default `hostUsers: false` (ADR-019 userns isolation),
                // but HONOR the spec override. k3s containerd doesn't
                // chown the pod cgroup under hostUsers:false → rio-
                // builder's `mkdir /sys/fs/cgroup/leaf` EACCES → exit 1
                // in <200ms (vmtest-full-nonpriv.yaml). The k3s VM tests
                // set `hostUsers: true`; production EKS (containerd 2.0+)
                // gets the default `false`. Forcing Some(false) here
                // (Phase-7 first cut) made fetcher pods unrunnable on
                // every CI fixture.
                host_users: pool.spec.host_users.or(Some(false)),
                image_pull_policy: pool.spec.image_pull_policy.clone(),
                // FODs route by is_fixed_output alone, not features.
                features: vec![],
                fuse_cache_quantity: Quantity(FETCHER_FUSE_CACHE.into()),
                fuse_threads: None,
                // Never privileged — fetchers face the open internet.
                privileged: false,
                // ADR-019 §Sandbox hardening: stricter Localhost
                // profile with extra denies (ptrace/bpf/setns/
                // process_vm_*/keyctl/add_key). Written by systemd-
                // tmpfiles on every node before kubelet starts.
                seccomp_profile: Some(SeccompProfileKind {
                    type_: "Localhost".into(),
                    localhost_profile: Some("operator/rio-fetcher.json".into()),
                }),
                host_network: None,
                // 10 minutes — fetches are short.
                termination_grace_period_seconds: pool
                    .spec
                    .termination_grace_period_seconds
                    .or(Some(600)),
            }
        }
    }
}

/// Labels applied to Jobs and pods for this pool.
pub(super) fn labels(pool: &Pool) -> BTreeMap<String, String> {
    pod::executor_labels(&executor_params(pool))
}

/// The pod spec. Re-exported for `jobs::build_job`.
pub(super) fn build_pod_spec(
    pool: &Pool,
    scheduler: &UpstreamAddrs,
    store: &UpstreamAddrs,
) -> PodSpec {
    pod::build_executor_pod_spec(&executor_params(pool), scheduler, store)
}
