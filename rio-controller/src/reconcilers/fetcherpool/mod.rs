//! FetcherPool reconciler: StatefulSet of rio-builder pods in
//! fetcher mode (`RIO_EXECUTOR_KIND=fetcher`).
//!
//! Minimal compared to [`builderpool`](super::builderpool): no
//! size-class (fetches are network-bound), no ephemeral mode, no
//! PDB, no autoscaler handshake. Just STS + headless Service +
//! status, with stricter security posture per ADR-019 §Sandbox
//! hardening.
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
use crate::crds::fetcherpool::FetcherPool;
use crate::error::{Error, Result, error_kind};
use crate::reconcilers::common::sts::{
    self, ExecutorRole, ExecutorStsParams, SchedulerAddrs, sts_name,
};
use crate::reconcilers::{Ctx, error_key};

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
    let ns = fp
        .namespace()
        .ok_or_else(|| Error::InvalidSpec("FetcherPool has no namespace".into()))?;
    let name = fp.name_any();
    let oref = fp.controller_owner_ref(&()).ok_or_else(|| {
        Error::InvalidSpec("FetcherPool has no metadata.uid (not from apiserver?)".into())
    })?;

    let params = executor_params(&fp)?;
    let labels = sts::executor_labels(&params);
    let sts_name = sts_name(&name, ExecutorRole::Fetcher);

    // ── Headless Service ────────────────────────────────────────
    let svc = Service {
        metadata: ObjectMeta {
            name: Some(sts_name.clone()),
            namespace: Some(ns.clone()),
            owner_references: Some(vec![oref.clone()]),
            labels: Some(labels.clone()),
            ..Default::default()
        },
        spec: Some(ServiceSpec {
            cluster_ip: Some("None".into()),
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
    Api::<Service>::namespaced(ctx.client.clone(), &ns)
        .patch(
            &sts_name,
            &PatchParams::apply(MANAGER).force(),
            &Patch::Apply(&svc),
        )
        .await?;

    // ── StatefulSet ─────────────────────────────────────────────
    // No autoscaler handshake — FetcherPool.spec.replicas is fixed.
    // Always send `replicas` (claim ownership; no other field
    // manager contends for it).
    let sts = sts::build_executor_statefulset(
        &params,
        oref,
        &SchedulerAddrs {
            addr: ctx.scheduler_addr.clone(),
            balance_host: ctx.scheduler_balance_host.clone(),
            balance_port: ctx.scheduler_balance_port,
        },
        &ctx.store_addr,
        Some(fp.spec.replicas),
    );
    let sts_api: Api<StatefulSet> = Api::namespaced(ctx.client.clone(), &ns);
    let applied = sts_api
        .patch(
            &sts_name,
            &PatchParams::apply(MANAGER).force(),
            &Patch::Apply(&sts),
        )
        .await?;

    // ── Status ──────────────────────────────────────────────────
    let sts_status = applied.status.unwrap_or_default();
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
                    "readyReplicas": sts_status.ready_replicas.unwrap_or(0),
                },
            })),
        )
        .await?;

    info!(pool = %name, "reconciled FetcherPool");
    Ok(Action::requeue(Duration::from_secs(300)))
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
fn executor_params(fp: &FetcherPool) -> Result<ExecutorStsParams> {
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
        extra_env: vec![],
        pool_name: fp.name_any(),
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
        // Network-bound, one fetch at a time. A future plan may
        // expose this as a spec field if parallel FOD fetching
        // proves useful.
        max_concurrent_builds: 1,
        resources: fp.spec.resources.clone(),
        fuse_cache_gb: cache_gb,
        fuse_cache_quantity: Quantity(FETCHER_FUSE_CACHE.into()),
        fuse_threads: None,
        // Never privileged — fetchers face the open internet; the
        // escape hatch stays closed.
        privileged: false,
        // ADR-019 §Sandbox hardening: stricter Localhost profile
        // with extra denies (ptrace/bpf/setns/process_vm_*/keyctl/
        // add_key).
        seccomp_profile: Some(SeccompProfileKind {
            type_: "Localhost".into(),
            localhost_profile: Some("profiles/rio-fetcher.json".into()),
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

    fn mk(replicas: i32) -> FetcherPool {
        let mut fp = FetcherPool::new(
            "test",
            crate::crds::fetcherpool::FetcherPoolSpec {
                replicas,
                image: "rio-builder:test".into(),
                systems: vec!["x86_64-linux".into()],
                node_selector: None,
                tolerations: None,
                resources: None,
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
        let fp = mk(2);
        let params = executor_params(&fp).unwrap();
        let labels = sts::executor_labels(&params);
        assert_eq!(labels.get("rio.build/role"), Some(&"fetcher".into()));
        assert_eq!(labels.get("rio.build/pool"), Some(&"test".into()));
    }

    /// `readOnlyRootFilesystem: true` + Localhost seccomp =
    /// `rio-fetcher.json`. ADR-019 §Sandbox hardening.
    #[test]
    fn security_posture_is_strict() {
        let fp = mk(1);
        let params = executor_params(&fp).unwrap();
        assert!(params.read_only_root_fs);
        assert!(!params.privileged);
        let sp = params.seccomp_profile.as_ref().unwrap();
        assert_eq!(sp.type_, "Localhost");
        assert_eq!(
            sp.localhost_profile.as_deref(),
            Some("profiles/rio-fetcher.json")
        );
    }

    /// Default nodeSelector + toleration target the dedicated
    /// fetcher node pool. ADR-019 §Node isolation.
    #[test]
    fn node_placement_defaults_to_fetcher_pool() {
        let fp = mk(1);
        let params = executor_params(&fp).unwrap();
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
        let params = executor_params(&fp).unwrap();
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

    /// STS name is `{pool}-fetchers` — distinct from builders.
    #[test]
    fn sts_name_has_role_suffix() {
        assert_eq!(
            sts_name("default", ExecutorRole::Fetcher),
            "default-fetchers"
        );
        assert_eq!(
            sts_name("default", ExecutorRole::Builder),
            "default-builders"
        );
    }
}
