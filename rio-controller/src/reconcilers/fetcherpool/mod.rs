//! FetcherPool reconciler: one-shot Jobs of rio-builder in fetcher
//! mode (`RIO_EXECUTOR_KIND=fetcher`), with the stricter security
//! posture from ADR-019 §Sandbox hardening.
//!
//! Optionally size-classed via `spec.classes[]` (I-170): one Job
//! loop per class, each registering `RIO_SIZE_CLASS=name` so the
//! scheduler can route by `size_class_floor`. Simpler than
//! [`builderpool`](super::builderpool): no manifest mode, no
//! duration-cutoff rebalancer.
// TODO: add the ctrl.fetcherpool.reconcile impl marker once
// ADR-019 is in tracey spec_include (the rule is defined in
// decisions/019 but tracey only scans components/ today).

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use k8s_openapi::api::core::v1::Toleration;
use k8s_openapi::apimachinery::pkg::api::resource::Quantity;
use kube::ResourceExt;
use kube::api::Api;
use kube::runtime::controller::Action;
use kube::runtime::finalizer::{Event, finalizer};
use tracing::{info, warn};

use crate::error::{Error, Result, error_kind};
use crate::reconcilers::common::pod::{self, ExecutorKind, ExecutorPodParams};
use crate::reconcilers::{Ctx, error_key};
use rio_crds::builderpool::SeccompProfileKind;
use rio_crds::fetcherpool::{FetcherPool, FetcherSizeClass};

mod ephemeral;

/// Finalizer name. Kubebuilder convention: `{kind}.{group}/{suffix}`.
const FINALIZER: &str = "fetcherpool.rio.build/drain";

/// Field manager for server-side apply.
const MANAGER: &str = "rio-controller";

/// Default FUSE cache size for fetchers. FODs are typically small
/// (source tarballs, git clones) — 10Gi is plenty. BuilderPool
/// exposes this as a spec field; FetcherPool hardcodes it.
const FETCHER_FUSE_CACHE: &str = "10Gi";

/// Top-level reconcile. Same finalizer-wrap pattern as
/// [`builderpool::reconcile`](super::builderpool::reconcile).
#[tracing::instrument(
    skip(fp, ctx),
    fields(reconciler = "fetcherpool", pool = %fp.name_any(), ns = fp.namespace().as_deref().unwrap_or(""))
)]
pub async fn reconcile(fp: Arc<FetcherPool>, ctx: Arc<Ctx>) -> Result<Action> {
    let start = std::time::Instant::now();
    let key = error_key(fp.as_ref());
    let result = reconcile_inner(fp, ctx.clone()).await;
    if result.is_ok() {
        ctx.reset_error_count(&key);
    }
    metrics::histogram!("rio_controller_reconcile_duration_seconds",
        "reconciler" => "fetcherpool")
    .record(start.elapsed().as_secs_f64());
    result
}

async fn reconcile_inner(fp: Arc<FetcherPool>, ctx: Arc<Ctx>) -> Result<Action> {
    let ns = fp
        .namespace()
        .ok_or_else(|| Error::InvalidSpec("FetcherPool has no namespace".into()))?;
    let api: Api<FetcherPool> = Api::namespaced(ctx.client.clone(), &ns);

    finalizer(&api, FINALIZER, fp, |event| async {
        match event {
            Event::Apply(fp) => apply(fp, &ctx).await,
            Event::Cleanup(fp) => cleanup(fp, &ctx).await,
        }
    })
    .await
    .map_err(|e| Error::Finalizer(Box::new(e)))
}

/// Normal reconcile: make the world match spec.
async fn apply(fp: Arc<FetcherPool>, ctx: &Ctx) -> Result<Action> {
    ephemeral::reconcile_ephemeral(&fp, ctx).await
}

/// Cleanup: ownerRef GC handles the Jobs. Fetches are short-lived
/// so there's no long terminationGracePeriod to wait through —
/// just let GC proceed.
async fn cleanup(fp: Arc<FetcherPool>, _ctx: &Ctx) -> Result<Action> {
    info!(pool = %fp.name_any(), "FetcherPool deleted; ownerRef GC will clean up");
    Ok(Action::await_change())
}

/// Convert `FetcherPool` → `ExecutorPodParams` with fetcher-specific
/// hardening defaults.
///
/// `class`: when `Some`, overrides `pool_name` suffix, `resources`,
/// and injects `RIO_SIZE_CLASS` so the executor reports it in
/// heartbeat (`r[ctrl.fetcherpool.classes]`). When `None`, single-
/// pool behavior at `spec.resources` (back-compat). Security
/// posture (read-only rootfs, seccomp, node placement) is identical
/// across classes — only resources + size_class env vary.
fn executor_params(
    fp: &FetcherPool,
    class: Option<&FetcherSizeClass>,
) -> Result<ExecutorPodParams> {
    // ADR-019 §Node isolation: fetchers land on dedicated nodes via
    // the `rio.build/fetcher=true:NoSchedule` taint + matching
    // selector. If the operator supplies their own, honor those
    // instead — lets them override for dev clusters without
    // dedicated node pools.
    // TODO(P0455): add the fetcher.node.dedicated impl marker here
    // once ADR-019 is in tracey spec_include.
    let node_selector = fp.spec.node_selector.clone().or_else(|| {
        Some(BTreeMap::from([(
            "rio.build/node-role".into(),
            "fetcher".into(),
        )]))
    });
    let tolerations = fp.spec.tolerations.clone().or_else(|| {
        Some(vec![Toleration {
            key: Some("rio.build/fetcher".into()),
            operator: Some("Exists".into()),
            effect: Some("NoSchedule".into()),
            ..Default::default()
        }])
    });

    Ok(ExecutorPodParams {
        role: ExecutorKind::Fetcher,
        // ADR-019 §Sandbox hardening: rootfs tampering blocked. The
        // overlay upperdir (tmpfs emptyDir in common/pod.rs) stays
        // writable so build outputs still land.
        // TODO(P0455): add the fetcher.sandbox.strict-seccomp impl
        // marker here (readOnlyRootFilesystem half) once ADR-019 is
        // in tracey spec_include.
        read_only_root_fs: true,
        // r[impl ctrl.fetcherpool.classes]
        // RIO_SIZE_CLASS: same env builders use (common/pod.rs reads
        // it from extra_env). The executor copies this into
        // HeartbeatRequest.size_class → ExecutorState.size_class →
        // hard_filter's size-class match clause. Unclassed pool
        // (`class=None`) leaves it empty → executor reports
        // size_class=None → hard_filter passes through (back-compat).
        extra_env: class
            .map(|c| vec![pod::env("RIO_SIZE_CLASS", &c.name)])
            .unwrap_or_default(),
        // r[impl ctrl.fetcherpool.multiarch]
        // Per-class pool_name → Job-name prefix
        // `rio-fetcher-{fp}-{class}`.
        // P0556 had dropped the fp-name segment ("fetchers are a
        // single pool by convention"); multi-arch FetcherPools break
        // that — two pools `x86-64` and `aarch64` with the same
        // `classes=[tiny]` would both stamp `rio-fetcher-tiny` in
        // the same namespace. Restoring `{fp.name}-{class.name}`
        // matches builder naming (`rio-builder-x86-64-tiny`).
        // executor_labels reads this for `rio.build/pool` so the
        // per-class headless Service selector and ephemeral
        // active-Job count stay per-pool. Unclassed (`class=None`)
        // keeps bare `fp.name_any()` for pre-I-170 back-compat.
        pool_name: match class {
            Some(c) => format!("{}-{}", fp.name_any(), c.name),
            None => fp.name_any(),
        },
        namespace: fp
            .namespace()
            .ok_or_else(|| Error::InvalidSpec("FetcherPool has no namespace".into()))?,
        node_selector,
        tolerations,
        image: fp.spec.image.clone(),
        image_pull_policy: None,
        systems: fp.spec.systems.clone(),
        // Fetchers don't advertise features — FODs route by
        // is_fixed_output alone, not by feature set.
        features: vec![],
        // Per-class resources override; CEL guarantees spec.resources
        // is None when classes is non-empty, so the `or` is for the
        // unclassed back-compat path.
        resources: class
            .map(|c| c.resources.clone())
            .or_else(|| fp.spec.resources.clone()),
        fuse_cache_quantity: Quantity(FETCHER_FUSE_CACHE.into()),
        fuse_threads: None,
        // Never privileged — fetchers face the open internet; the
        // escape hatch stays closed.
        privileged: false,
        // ADR-019 §Sandbox hardening: stricter Localhost profile
        // with extra denies (ptrace/bpf/setns/process_vm_*/keyctl/
        // add_key). Written by systemd-tmpfiles on every node before
        // kubelet starts (nix/nixos-node/hardening.nix on EKS;
        // fixtures/k3s-full.nix in VM tests).
        seccomp_profile: Some(SeccompProfileKind {
            type_: "Localhost".into(),
            localhost_profile: Some("operator/rio-fetcher.json".into()),
        }),
        host_network: None,
        host_users: fp.spec.host_users,
        // Same mTLS client cert as builders — same binary, same
        // scheduler/store endpoints. Without this, the fetcher's
        // heartbeat is rejected in mTLS deployments (the scheduler
        // requires a cert chaining to the shared CA).
        tls_secret_name: fp.spec.tls_secret_name.clone(),
        // 10 minutes — fetches are short. The builder default of
        // 2h is for LLVM-scale builds.
        termination_grace_period_seconds: Some(600),
    })
}

/// Requeue policy. Same curve as builderpool — exponential backoff
/// for transients, slow requeue for InvalidSpec.
pub fn error_policy(fp: Arc<FetcherPool>, err: &Error, ctx: Arc<Ctx>) -> Action {
    metrics::counter!("rio_controller_reconcile_errors_total",
        "reconciler" => "fetcherpool", "error_kind" => error_kind(err))
    .increment(1);

    match err {
        Error::InvalidSpec(msg) => {
            warn!(error = %msg, "invalid FetcherPool spec; fix the CRD");
            Action::requeue(Duration::from_secs(300))
        }
        _ => {
            let delay = ctx.error_backoff(&error_key(fp.as_ref()));
            warn!(error = %err, backoff = ?delay, "reconcile failed; retrying");
            Action::requeue(delay)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use k8s_openapi::api::core::v1::ResourceRequirements;

    fn mk(max: u32) -> FetcherPool {
        let mut fp = FetcherPool::new(
            "test",
            rio_crds::fetcherpool::FetcherPoolSpec {
                common: rio_crds::common::PoolSpecCommon {
                    deadline_seconds: None,
                    max_concurrent: max,
                    image: "rio-builder:test".into(),
                    systems: vec!["x86_64-linux".into()],
                    node_selector: None,
                    tolerations: None,
                    resources: None,
                    tls_secret_name: None,
                    host_users: None,
                },
                classes: vec![],
            },
        );
        fp.metadata.namespace = Some("rio-fetchers".into());
        fp
    }

    /// Generated Job pods carry `rio.build/role: fetcher` on the
    /// pod template labels — NetworkPolicies and `kubectl get -l`
    /// target this.
    #[test]
    fn labels_include_fetcher_role() {
        let fp = mk(8);
        let params = executor_params(&fp, None).unwrap();
        let labels = pod::executor_labels(&params);
        assert_eq!(labels.get("rio.build/role"), Some(&"fetcher".into()));
        assert_eq!(labels.get("rio.build/pool"), Some(&"test".into()));
    }

    /// `readOnlyRootFilesystem: true` + Localhost seccomp =
    /// `rio-fetcher.json`. ADR-019 §Sandbox hardening.
    #[test]
    fn security_posture_is_strict() {
        let fp = mk(1);
        let params = executor_params(&fp, None).unwrap();
        assert!(params.read_only_root_fs);
        assert!(!params.privileged);
        let sp = params.seccomp_profile.as_ref().unwrap();
        assert_eq!(sp.type_, "Localhost");
        assert_eq!(
            sp.localhost_profile.as_deref(),
            Some("operator/rio-fetcher.json")
        );
    }

    /// Default nodeSelector + toleration target the dedicated
    /// fetcher node pool. ADR-019 §Node isolation.
    #[test]
    fn node_placement_defaults_to_fetcher_pool() {
        let fp = mk(1);
        let params = executor_params(&fp, None).unwrap();
        assert_eq!(
            params
                .node_selector
                .as_ref()
                .unwrap()
                .get("rio.build/node-role"),
            Some(&"fetcher".into())
        );
        let tol = &params.tolerations.as_ref().unwrap()[0];
        assert_eq!(tol.key.as_deref(), Some("rio.build/fetcher"));
        assert_eq!(tol.effect.as_deref(), Some("NoSchedule"));
    }

    /// Operator-supplied nodeSelector/tolerations override the
    /// defaults — dev clusters without dedicated pools.
    #[test]
    fn operator_placement_overrides_default() {
        let mut fp = mk(1);
        fp.spec.node_selector = Some(BTreeMap::from([("custom".into(), "yes".into())]));
        let params = executor_params(&fp, None).unwrap();
        assert_eq!(
            params.node_selector.as_ref().unwrap().get("custom"),
            Some(&"yes".into())
        );
        assert!(
            !params
                .node_selector
                .as_ref()
                .unwrap()
                .contains_key("rio.build/node-role")
        );
    }

    // r[verify ctrl.fetcherpool.classes]
    /// I-170: per-class params carry `RIO_SIZE_CLASS=<name>`, the
    /// per-class resources, and a `{pool}-{class}` pool_name
    /// (→ Job-name prefix `rio-fetcher-{pool}-{class}`). Security
    /// posture is identical to the unclassed path.
    #[test]
    fn per_class_params_set_size_class_and_resources() {
        use k8s_openapi::apimachinery::pkg::api::resource::Quantity;
        let fp = mk(8);
        let class: FetcherSizeClass = rio_crds::common::SizeClassCommon {
            name: "small".into(),
            resources: ResourceRequirements {
                limits: Some(BTreeMap::from([("memory".into(), Quantity("8Gi".into()))])),
                ..Default::default()
            },
            max_concurrent: Some(4),
        }
        .into();
        let params = executor_params(&fp, Some(&class)).unwrap();
        // RIO_SIZE_CLASS injected via extra_env (sts.rs appends it
        // after the base env set).
        let env: BTreeMap<_, _> = params
            .extra_env
            .iter()
            .filter_map(|e| Some((e.name.as_str(), e.value.as_deref()?)))
            .collect();
        assert_eq!(env.get("RIO_SIZE_CLASS"), Some(&"small"));
        // pool_name = `{fp.name}-{class.name}` so per-arch pools
        // don't collide (multiarch_pools_distinct_job_names below).
        assert_eq!(params.pool_name, "test-small");
        assert_eq!(
            pod::job_name(&params.pool_name, ExecutorKind::Fetcher, "abc123"),
            "rio-fetcher-test-small-abc123"
        );
        // Per-class resources, NOT spec.resources (which is None).
        assert_eq!(
            params
                .resources
                .as_ref()
                .and_then(|r| r.limits.as_ref())
                .and_then(|l| l.get("memory")),
            Some(&Quantity("8Gi".into()))
        );
        // Security posture unchanged across classes — only
        // resources + size_class env vary.
        assert!(params.read_only_root_fs);
        assert!(!params.privileged);
    }

    // r[verify ctrl.fetcherpool.classes]
    /// I-170: a FetcherPool with `classes=[tiny, small]` produces two
    /// distinct pool_names → distinct Job-name prefixes. The reconciler
    /// iterates `spec.classes` and spawns one Job loop per class.
    #[test]
    fn classes_produce_distinct_job_names() {
        let mut fp = mk(8);
        fp.spec.classes = vec![
            rio_crds::common::SizeClassCommon {
                name: "tiny".into(),
                resources: ResourceRequirements::default(),
                max_concurrent: None,
            }
            .into(),
            rio_crds::common::SizeClassCommon {
                name: "small".into(),
                resources: ResourceRequirements::default(),
                max_concurrent: None,
            }
            .into(),
        ];
        let names: Vec<_> = fp
            .spec
            .classes
            .iter()
            .map(|c| executor_params(&fp, Some(c)).unwrap().pool_name)
            .collect();
        assert_eq!(names, vec!["test-tiny", "test-small"]);
    }

    // r[verify ctrl.fetcherpool.multiarch]
    /// Two FetcherPools (one per arch) with the same `classes=[tiny]`
    /// produce DISTINCT pool_names. P0556's `pool_name = class.name`
    /// would collide both at `tiny`; the `{fp}-{class}` form keeps
    /// them separate. Mirrors `rio-builder-{arch}-{class}`.
    #[test]
    fn multiarch_pools_distinct_job_names() {
        let class: FetcherSizeClass = rio_crds::common::SizeClassCommon {
            name: "tiny".into(),
            resources: ResourceRequirements::default(),
            max_concurrent: None,
        }
        .into();
        let mut x86 = mk(8);
        x86.metadata.name = Some("x86-64".into());
        let mut arm = mk(8);
        arm.metadata.name = Some("aarch64".into());

        let n = |fp: &FetcherPool| executor_params(fp, Some(&class)).unwrap().pool_name;
        assert_eq!(n(&x86), "x86-64-tiny");
        assert_eq!(n(&arm), "aarch64-tiny");
        assert_ne!(n(&x86), n(&arm), "per-arch pools must not collide");
        // Max length headroom: job_name `rio-fetcher-aarch64-small-abcdef`
        // = 32 chars; RFC 1123 limit is 63.
        assert!(pod::job_name(&n(&arm), ExecutorKind::Fetcher, "abcdef").len() < 63);
    }

    /// Unclassed path: `class=None` → no RIO_SIZE_CLASS env, bare
    /// pool_name. Back-compat with pre-I-170 FetcherPools.
    #[test]
    fn unclassed_params_no_size_class_env() {
        let fp = mk(8);
        let params = executor_params(&fp, None).unwrap();
        assert!(
            params.extra_env.is_empty(),
            "unclassed → executor reports size_class=None → hard_filter passes through"
        );
        assert_eq!(params.pool_name, "test");
    }

    /// Job name is `rio-{role}-{pool}-{suffix}` (I-104) — pool name
    /// is the disambiguating part (typically arch).
    #[test]
    fn job_name_has_role_then_pool_then_suffix() {
        assert_eq!(
            pod::job_name("default", ExecutorKind::Fetcher, "abc"),
            "rio-fetcher-default-abc"
        );
        assert_eq!(
            pod::job_name("x86-64", ExecutorKind::Builder, "abc"),
            "rio-builder-x86-64-abc"
        );
    }
}
