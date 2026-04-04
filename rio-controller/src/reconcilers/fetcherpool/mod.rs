//! FetcherPool reconciler: rio-builder pods in fetcher mode
//! (`RIO_EXECUTOR_KIND=fetcher`).
//!
//! Two modes (P0541): `ephemeral: true` (default) → Job-per-FOD via
//! the `ephemeral` submodule; `ephemeral: false` → StatefulSet + headless
//! Service, autoscaled on `queued_fod_derivations`. Both apply the
//! stricter security posture per ADR-019 §Sandbox hardening.
//!
//! Optionally size-classed via `spec.classes[]` (I-170): when
//! non-empty, one StatefulSet (or ephemeral Job loop) per class;
//! each registers `RIO_SIZE_CLASS=name` so the scheduler can route
//! by `size_class_floor`. Still simpler than
//! [`builderpool`](super::builderpool): no PDB, no manifest mode,
//! no duration-cutoff rebalancer.
// TODO(P0455): add the ctrl.fetcherpool.reconcile impl marker once
// ADR-019 is in tracey spec_include (the rule is defined in
// decisions/019 but tracey only scans components/ today).

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use k8s_openapi::api::apps::v1::StatefulSet;
use k8s_openapi::api::core::v1::{Service, ServicePort, ServiceSpec, Toleration};
use k8s_openapi::apimachinery::pkg::api::resource::Quantity;
use kube::api::{Api, ObjectMeta, Patch, PatchParams};
use kube::runtime::controller::Action;
use kube::runtime::finalizer::{Event, finalizer};
use kube::{CustomResourceExt, Resource, ResourceExt};
use tracing::{info, warn};

use crate::crds::builderpool::SeccompProfileKind;
use crate::crds::fetcherpool::{FetcherPool, FetcherSizeClass};
use crate::error::{Error, Result, error_kind};
use crate::reconcilers::common::sts::{self, ExecutorRole, ExecutorStsParams, sts_name};
use crate::reconcilers::{Ctx, error_key};

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
    if fp.spec.ephemeral {
        return ephemeral::reconcile_ephemeral(&fp, ctx).await;
    }

    let ns = fp
        .namespace()
        .ok_or_else(|| Error::InvalidSpec("FetcherPool has no namespace".into()))?;
    let name = fp.name_any();
    let oref = fp.controller_owner_ref(&()).ok_or_else(|| {
        Error::InvalidSpec("FetcherPool has no metadata.uid (not from apiserver?)".into())
    })?;

    // r[impl ctrl.fetcherpool.classes]
    // I-170: when `classes` is non-empty, stamp one STS+Service per
    // class. When empty (back-compat), single STS at `spec.resources`.
    // Status aggregates ready/desired across classes.
    let mut total_ready = 0i32;
    let mut total_desired = 0i32;
    if fp.spec.classes.is_empty() {
        let (ready, desired) = apply_one(&fp, ctx, &ns, &oref, &name, None).await?;
        total_ready += ready;
        total_desired += desired;
    } else {
        for class in &fp.spec.classes {
            // P0556: pool_name = class.name (fp-name segment dropped —
            // fetchers are a single pool by convention). Matches
            // executor_params() so STS name and `rio.build/pool` label
            // agree.
            let (ready, desired) =
                apply_one(&fp, ctx, &ns, &oref, &class.name, Some(class)).await?;
            total_ready += ready;
            total_desired += desired;
        }
    }

    // ── Status ──────────────────────────────────────────────────
    // Partial: reconciler owns readyReplicas/desiredReplicas;
    // autoscaler owns lastScaleTime/conditions via a separate
    // field-manager. Same SSA split as builderpool.
    let fp_api: Api<FetcherPool> = Api::namespaced(ctx.client.clone(), &ns);
    let ar = FetcherPool::api_resource();
    fp_api
        .patch_status(
            &name,
            &PatchParams::apply(MANAGER).force(),
            &Patch::Apply(serde_json::json!({
                "apiVersion": ar.api_version,
                "kind": ar.kind,
                "status": {
                    "readyReplicas": total_ready,
                    "desiredReplicas": total_desired,
                },
            })),
        )
        .await?;

    info!(pool = %name, classes = fp.spec.classes.len(), "reconciled FetcherPool");
    Ok(Action::requeue(Duration::from_secs(300)))
}

/// Apply one STS+Service for a single size-class (or the unclassed
/// pool when `class` is `None`). Returns `(ready, desired)` from
/// the resulting STS for status aggregation.
async fn apply_one(
    fp: &FetcherPool,
    ctx: &Ctx,
    ns: &str,
    oref: &k8s_openapi::apimachinery::pkg::apis::meta::v1::OwnerReference,
    pool_name: &str,
    class: Option<&FetcherSizeClass>,
) -> Result<(i32, i32)> {
    let params = executor_params(fp, class)?;
    let labels = sts::executor_labels(&params);
    let sts_name = sts_name(pool_name, ExecutorRole::Fetcher);

    // ── Headless Service ────────────────────────────────────────
    let svc = Service {
        metadata: ObjectMeta {
            name: Some(sts_name.clone()),
            namespace: Some(ns.into()),
            owner_references: Some(vec![oref.clone()]),
            labels: Some(labels.clone()),
            ..Default::default()
        },
        spec: Some(ServiceSpec {
            cluster_ip: Some("None".into()),
            // I-085: omit ip_families (single-stack-safe; see 46b3c590).
            ip_family_policy: Some("PreferDualStack".into()),
            selector: Some(labels),
            ports: Some(vec![ServicePort {
                name: Some("metrics".into()),
                port: 9093,
                ..Default::default()
            }]),
            ..Default::default()
        }),
        ..Default::default()
    };
    Api::<Service>::namespaced(ctx.client.clone(), ns)
        .patch(
            &sts_name,
            &PatchParams::apply(MANAGER).force(),
            &Patch::Apply(&svc),
        )
        .await?;

    // ── StatefulSet ─────────────────────────────────────────────
    // Autoscaler handshake (same SSA dance as builderpool/mod.rs):
    // set replicas only on first create, omit on subsequent
    // reconciles. The autoscaler ("rio-controller-autoscaler") owns
    // `spec.replicas` after that; sending it here with .force()
    // would revert every scale decision back to min.
    let sts_api: Api<StatefulSet> = Api::namespaced(ctx.client.clone(), ns);
    let existing = sts_api.get_opt(&sts_name).await?;
    // Per-class min override falls back to pool-wide replicas.min.
    let min = class
        .and_then(|c| c.min_replicas)
        .unwrap_or(fp.spec.replicas.min);
    let initial_replicas = existing.is_none().then_some(min);
    // For status.desiredReplicas: read what's actually on the STS
    // (autoscaler's last decision, or min on first create).
    let current_replicas = existing
        .as_ref()
        .and_then(|s| s.spec.as_ref())
        .and_then(|s| s.replicas)
        .unwrap_or(min);

    let sts = sts::build_executor_statefulset(
        &params,
        oref.clone(),
        &ctx.scheduler_addrs(),
        &ctx.store_addrs(),
        initial_replicas,
    );
    let applied = sts_api
        .patch(
            &sts_name,
            &PatchParams::apply(MANAGER).force(),
            &Patch::Apply(&sts),
        )
        .await?;

    let sts_status = applied.status.unwrap_or_default();
    Ok((sts_status.ready_replicas.unwrap_or(0), current_replicas))
}

/// Cleanup: ownerRef GC handles the STS + Service. Fetches are
/// short-lived so there's no long terminationGracePeriod to wait
/// through — just let GC proceed.
async fn cleanup(fp: Arc<FetcherPool>, _ctx: &Ctx) -> Result<Action> {
    info!(pool = %fp.name_any(), "FetcherPool deleted; ownerRef GC will clean up");
    Ok(Action::await_change())
}

/// Convert `FetcherPool` → `ExecutorStsParams` with fetcher-specific
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
) -> Result<ExecutorStsParams> {
    let cache_gb = sts::parse_quantity_to_gb(FETCHER_FUSE_CACHE)?;

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

    Ok(ExecutorStsParams {
        role: ExecutorRole::Fetcher,
        // ADR-019 §Sandbox hardening: rootfs tampering blocked. The
        // overlay upperdir (tmpfs emptyDir in common/sts.rs) stays
        // writable so build outputs still land.
        // TODO(P0455): add the fetcher.sandbox.strict-seccomp impl
        // marker here (readOnlyRootFilesystem half) once ADR-019 is
        // in tracey spec_include.
        read_only_root_fs: true,
        // r[impl ctrl.fetcherpool.classes]
        // RIO_SIZE_CLASS: same env builders use (sts.rs:710 reads it
        // from extra_env). The executor copies this into
        // HeartbeatRequest.size_class → ExecutorState.size_class →
        // hard_filter's size-class match clause. Unclassed pool
        // (`class=None`) leaves it empty → executor reports
        // size_class=None → hard_filter passes through (back-compat).
        extra_env: class
            .map(|c| vec![sts::env("RIO_SIZE_CLASS", &c.name)])
            .unwrap_or_default(),
        // Per-class pool_name → STS name `rio-fetcher-{class}`.
        // P0556: drop the FetcherPool name segment — fetchers are a
        // single pool by convention (unlike per-arch BuilderPools), so
        // `rio-fetcher-default-tiny` was carrying a vestigial
        // `default`. executor_labels reads this for `rio.build/pool`
        // so the per-class headless Service selector and ephemeral
        // active-Job count match. Unclassed (`class=None`) keeps
        // `fp.name_any()` for back-compat with pre-I-170 deployments.
        pool_name: match class {
            Some(c) => c.name.clone(),
            None => fp.name_any(),
        },
        namespace: fp
            .namespace()
            .ok_or_else(|| Error::InvalidSpec("FetcherPool has no namespace".into()))?,
        node_selector,
        tolerations,
        topology_spread: Some(true),
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
        fuse_cache_gb: cache_gb,
        fuse_cache_quantity: Quantity(FETCHER_FUSE_CACHE.into()),
        fuse_threads: None,
        // Never privileged — fetchers face the open internet; the
        // escape hatch stays closed.
        privileged: false,
        // ADR-019 §Sandbox hardening: stricter Localhost profile
        // with extra denies (ptrace/bpf/setns/process_vm_*/keyctl/
        // add_key). The SeccompProfile CR is cluster-scoped (SPO
        // ≥0.9.0), so the path is operator/{name}.json with no
        // namespace component.
        seccomp_profile: Some(SeccompProfileKind {
            type_: "Localhost".into(),
            localhost_profile: Some("operator/rio-fetcher.json".into()),
        }),
        seccomp_preinstalled: sts::seccomp_preinstalled(),
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

    fn mk(min: i32, max: i32) -> FetcherPool {
        use crate::crds::builderpool::{Autoscaling, Replicas};
        let mut fp = FetcherPool::new(
            "test",
            crate::crds::fetcherpool::FetcherPoolSpec {
                ephemeral: false,
                ephemeral_deadline_seconds: None,
                replicas: Replicas { min, max },
                autoscaling: Autoscaling {
                    metric: "fodQueueDepth".into(),
                    target_value: 5,
                },
                image: "rio-builder:test".into(),
                systems: vec!["x86_64-linux".into()],
                node_selector: None,
                tolerations: None,
                resources: None,
                classes: vec![],
                tls_secret_name: None,
                host_users: None,
            },
        );
        fp.metadata.namespace = Some("rio-fetchers".into());
        fp
    }

    /// The generated STS carries `rio.build/role: fetcher` on
    /// both pod template labels and selector — NetworkPolicies
    /// and `kubectl get -l` target this.
    #[test]
    fn labels_include_fetcher_role() {
        let fp = mk(2, 8);
        let params = executor_params(&fp, None).unwrap();
        let labels = sts::executor_labels(&params);
        assert_eq!(labels.get("rio.build/role"), Some(&"fetcher".into()));
        assert_eq!(labels.get("rio.build/pool"), Some(&"test".into()));
    }

    /// `readOnlyRootFilesystem: true` + Localhost seccomp =
    /// `rio-fetcher.json`. ADR-019 §Sandbox hardening.
    #[test]
    fn security_posture_is_strict() {
        let fp = mk(1, 1);
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
        let fp = mk(1, 1);
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
        let mut fp = mk(1, 1);
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
    /// per-class resources, and a `{pool}-{class}` pool_name (→ STS
    /// name `rio-fetcher-{pool}-{class}`). Security posture is
    /// identical to the unclassed path.
    #[test]
    fn per_class_params_set_size_class_and_resources() {
        use k8s_openapi::apimachinery::pkg::api::resource::Quantity;
        let fp = mk(2, 8);
        let class = FetcherSizeClass {
            name: "small".into(),
            resources: ResourceRequirements {
                limits: Some(BTreeMap::from([("memory".into(), Quantity("8Gi".into()))])),
                ..Default::default()
            },
            min_replicas: Some(0),
            max_replicas: Some(4),
        };
        let params = executor_params(&fp, Some(&class)).unwrap();
        // RIO_SIZE_CLASS injected via extra_env (sts.rs appends it
        // after the base env set).
        let env: BTreeMap<_, _> = params
            .extra_env
            .iter()
            .filter_map(|e| Some((e.name.as_str(), e.value.as_deref()?)))
            .collect();
        assert_eq!(env.get("RIO_SIZE_CLASS"), Some(&"small"));
        // P0556: pool_name is just the class name (fp-name segment
        // dropped — fetchers are a single pool by convention).
        assert_eq!(params.pool_name, "small");
        assert_eq!(
            sts_name(&params.pool_name, ExecutorRole::Fetcher),
            "rio-fetcher-small"
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
    /// distinct STS names. The reconciler iterates `spec.classes` and
    /// stamps one STS+Service per class; this verifies the name
    /// derivation (the apply loop itself needs a kube-apiserver mock).
    #[test]
    fn classes_produce_distinct_sts_names() {
        let mut fp = mk(2, 8);
        fp.spec.classes = vec![
            FetcherSizeClass {
                name: "tiny".into(),
                resources: ResourceRequirements::default(),
                min_replicas: None,
                max_replicas: None,
            },
            FetcherSizeClass {
                name: "small".into(),
                resources: ResourceRequirements::default(),
                min_replicas: None,
                max_replicas: None,
            },
        ];
        let names: Vec<_> = fp
            .spec
            .classes
            .iter()
            .map(|c| {
                sts_name(
                    &executor_params(&fp, Some(c)).unwrap().pool_name,
                    ExecutorRole::Fetcher,
                )
            })
            .collect();
        assert_eq!(names, vec!["rio-fetcher-tiny", "rio-fetcher-small"]);
    }

    /// Unclassed path: `class=None` → no RIO_SIZE_CLASS env, bare
    /// pool_name. Back-compat with pre-I-170 FetcherPools.
    #[test]
    fn unclassed_params_no_size_class_env() {
        let fp = mk(2, 8);
        let params = executor_params(&fp, None).unwrap();
        assert!(
            params.extra_env.is_empty(),
            "unclassed → executor reports size_class=None → hard_filter passes through"
        );
        assert_eq!(params.pool_name, "test");
    }

    /// STS name is `rio-{role}-{pool}` (I-104) — pool name is the
    /// disambiguating suffix (typically arch).
    #[test]
    fn sts_name_has_role_then_pool_suffix() {
        assert_eq!(
            sts_name("default", ExecutorRole::Fetcher),
            "rio-fetcher-default"
        );
        assert_eq!(
            sts_name("x86-64", ExecutorRole::Builder),
            "rio-builder-x86-64"
        );
        assert_eq!(
            sts_name("aarch64", ExecutorRole::Builder),
            "rio-builder-aarch64"
        );
    }
}
