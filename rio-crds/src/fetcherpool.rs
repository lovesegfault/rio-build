//! FetcherPool CRD: StatefulSet of rio-builder pods in fetcher mode
//! (`RIO_EXECUTOR_KIND=fetcher`).
//!
//! Fetchers run FOD-only (fixed-output derivations — source tarballs,
//! git clones) with open internet egress. The FOD hash check is the
//! integrity boundary: a tampered fetch produces a hash mismatch that
//! `verify_fod_hashes()` rejects before upload. See ADR-019.

use std::collections::BTreeMap;

use k8s_openapi::api::core::v1::{ResourceRequirements, Toleration};
use k8s_openapi::apimachinery::pkg::apis::meta::v1::{Condition, Time};
use kube::{CustomResource, KubeSchema};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::builderpool::Autoscaling;

/// FetcherPool spec. The reconciler labels pods `rio.build/role:
/// fetcher` and sets stricter `securityContext` (`readOnlyRootFilesystem:
/// true`, stricter seccomp).
///
/// Optionally size-classed via `classes[]` (I-170): fetchers run
/// arbitrary code (the FOD's `builder` script) and a large source
/// unpack+NAR-serialize can OOM a 2Gi pod. Unlike BuilderPoolSet
/// there is no a-priori signal (no `build_samples` history, no
/// duration cutoff) — the scheduler routes FODs to the smallest
/// class by default and reactively promotes on transient failure
/// (`r[sched.fod.size-class-reactive]`).
///
/// `KubeSchema` alongside `CustomResource`: same pattern as
/// BuilderPoolSpec. No CEL on this struct yet, but KubeSchema
/// keeps the door open without a re-derive.
#[derive(CustomResource, Deserialize, Serialize, Clone, Debug, KubeSchema)]
#[kube(
    group = "rio.build",
    version = "v1alpha1",
    kind = "FetcherPool",
    namespaced,
    status = "FetcherPoolStatus",
    shortname = "fp",
    printcolumn = r#"{"name":"Ready","type":"integer","jsonPath":".status.readyReplicas"}"#,
    printcolumn = r#"{"name":"Desired","type":"integer","jsonPath":".status.desiredReplicas"}"#,
    printcolumn = r#"{"name":"Age","type":"date","jsonPath":".metadata.creationTimestamp"}"#
)]
#[x_kube(
    validation = Rule::new("self.maxConcurrent > 0").message(
        "maxConcurrent must be > 0 — it is the concurrent-Job ceiling"
    )
)]
// CEL: classes[] and resources are mutually exclusive. When classes is
// non-empty, per-class resources apply; the top-level field would be
// ambiguous (which class does it size?). When classes is empty, the
// single STS uses top-level resources — current behavior, back-compat.
#[x_kube(
    validation = Rule::new(
        "size(self.classes) == 0 || !has(self.resources)"
    ).message(
        "classes[] and resources are mutually exclusive — set per-class resources in classes[].resources, or omit classes[] for a single-size pool"
    )
)]
#[serde(rename_all = "camelCase")]
pub struct FetcherPoolSpec {
    /// Backstop `activeDeadlineSeconds` on Jobs. Same field as
    /// BuilderPool. Default 300 (`reconcilers/fetcherpool/
    /// ephemeral.rs::FOD_DEADLINE_SECS`) — fetches are network-bound
    /// and short; a stuck download is the failure mode, not a 90min
    /// compile.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub deadline_seconds: Option<u32>,

    /// Concurrent-Job ceiling. The reconciler spawns one Job per
    /// outstanding FOD up to this many active at once. FODs spike at
    /// build start (40+ ready simultaneously when chains hit FOD-phase
    /// together) but are short (~seconds each).
    pub max_concurrent: u32,

    /// Autoscaling policy. Same struct as BuilderPool. The expected
    /// `metric` is `"fodQueueDepth"` (`ClusterStatus.queued_fod_
    /// derivations`); the controller's autoscaler validates and
    /// surfaces unrecognized metrics in `.status.conditions`.
    pub autoscaling: Autoscaling,

    /// Container image. Same binary as builders (`rio-builder`),
    /// different `RIO_EXECUTOR_KIND` env baked into the pod spec.
    pub image: String,

    /// Target systems (e.g., `["x86_64-linux"]`). Fetchers still
    /// execute the FOD's `builder` script (curl, git, etc.), so the
    /// system must match.
    pub systems: Vec<String>,

    /// Node selector. Pairs with `rio.build/node-role: fetcher`
    /// label on the Karpenter NodePool (spec: fetcher.node.dedicated).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub node_selector: Option<BTreeMap<String, String>>,

    /// Tolerations. Pairs with `rio.build/fetcher=true:NoSchedule`
    /// taint on fetcher nodes. Typed `Toleration` (not
    /// serde_json::Value); `any_object_array` passthrough because
    /// k8s-openapi types don't impl JsonSchema.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(schema_with = "crate::any_object_array")]
    pub tolerations: Option<Vec<Toleration>>,

    /// K8s resource requests/limits for a single-size pool. Fetchers
    /// are typically lighter than builders (network-bound) — 500m/1Gi
    /// default. Mutually exclusive with `classes[]` (CEL-enforced):
    /// when `classes` is non-empty, per-class resources apply and this
    /// field MUST be unset. `any_object` passthrough — see
    /// builderpool.rs for why.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(schema_with = "crate::any_object")]
    pub resources: Option<ResourceRequirements>,

    /// Optional size classes (I-170). When empty (default), single
    /// STS/Job-set at `spec.resources` — original behavior. When
    /// non-empty, the reconciler stamps one StatefulSet (or
    /// ephemeral Job loop) per class; each registers with
    /// `size_class = name` so the scheduler can route by
    /// `DerivationState.size_class_floor`.
    ///
    /// Unlike `BuilderPoolSet.classes[]` there is NO `cutoff_secs` —
    /// fetchers have no a-priori duration estimate (FODs are excluded
    /// from `build_samples`; ADR-019). Routing is reactive only: a
    /// FOD that fails on class N retries on class N+1. Order classes
    /// smallest→largest; the scheduler treats `classes[0]` as the
    /// default floor.
    ///
    /// NOT a separate `FetcherPoolSet` CRD: the BuilderPoolSet→child
    /// indirection exists for per-class autoscaling targets keyed on
    /// per-class queue depth. Fetcher classes are expected to be
    /// `[tiny, small]` with `small` rarely used — flat `classes[]`
    /// on the existing CRD is simpler. Per-arch is handled by
    /// rendering multiple `FetcherPool` CRs (helm `fetcherPools[]`),
    /// not by a *Set CRD. Promote to a *Set CRD later if per-class-
    /// per-arch autoscaling targets diverge.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub classes: Vec<FetcherSizeClass>,

    /// mTLS Secret name (tls.crt/tls.key/ca.crt) for outgoing gRPC
    /// to scheduler + store. Same cert as builders (same binary,
    /// same client role). If unset, the fetcher runs without TLS
    /// and the scheduler rejects the heartbeat in mTLS deployments.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tls_secret_name: Option<String>,

    /// Explicit `hostUsers` override — same semantics as BuilderPool.
    /// `None` defaults to `hostUsers: false` (userns isolation). Set
    /// `true` for k3s/containerd deployments that don't chown the pod
    /// cgroup to the userns-mapped root UID (rio-builder's `mkdir
    /// /sys/fs/cgroup/leaf` fails EACCES → CrashLoopBackOff). See
    /// builderpool.rs's `host_users` doc for the full diagnostic.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub host_users: Option<bool>,
}

/// One fetcher size class. Mirrors `SizeClassSpec` minus
/// `cutoff_secs` — fetchers route by reactive floor only, not
/// duration estimate. The scheduler's `[[fetcher_size_classes]]`
/// config carries just the names (for ordering); per-class
/// `resources` live here on the controller side.
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct FetcherSizeClass {
    /// Class name. Becomes the StatefulSet name suffix
    /// (`rio-fetcher-{name}`) AND the `RIO_SIZE_CLASS` env the
    /// executor reports in its heartbeat. Convention: "tiny" /
    /// "small"; nothing enforces that.
    pub name: String,

    /// K8s resource requests/limits for this class's fetcher pods.
    /// NON-Option: distinct resource profiles are the entire point
    /// of size classes. `any_object` passthrough — see
    /// builderpool.rs for why.
    #[schemars(schema_with = "crate::any_object")]
    pub resources: ResourceRequirements,

    /// Concurrent-Job ceiling for this class. `None` = inherit
    /// `spec.max_concurrent`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_concurrent: Option<u32>,
}

/// FetcherPool status. Reconciler + autoscaler write; `kubectl get fp`
/// reads. Same SSA field-manager split as BuilderPool: reconciler owns
/// `readyReplicas`/`desiredReplicas`, autoscaler owns `lastScaleTime`/
/// `conditions`.
#[derive(Deserialize, Serialize, Clone, Debug, Default, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct FetcherPoolStatus {
    /// StatefulSet `.status.readyReplicas`. Passed readinessProbe
    /// = heartbeating to scheduler as `ExecutorKind::Fetcher`.
    #[serde(default)]
    pub ready_replicas: i32,
    /// What the autoscaler set on `StatefulSet.spec.replicas` (or
    /// `min` on first create). Reconciler reads this back from the
    /// STS so the printcolumn tracks autoscaler decisions.
    #[serde(default)]
    pub desired_replicas: i32,
    /// Last actual replica patch. For stabilization-window
    /// observability.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(with = "Option<String>")]
    pub last_scale_time: Option<Time>,
    /// Standard K8s Conditions. One type, `Scaling`: reason
    /// ScaledUp / ScaledDown / Stabilizing / UnknownMetric.
    #[serde(default)]
    #[schemars(schema_with = "crate::any_object_array")]
    pub conditions: Vec<Condition>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use kube::CustomResourceExt;

    /// The CRD serializes without panic. Same smoke check as
    /// builderpool.rs — catches schemars derive or #[kube] attr
    /// misconfiguration at `cargo test` time, not at crdgen run.
    #[test]
    fn crd_serializes() {
        let crd = FetcherPool::crd();
        let yaml = serde_yml::to_string(&crd).expect("serializes");
        assert!(yaml.contains("group: rio.build"));
        assert!(yaml.contains("kind: FetcherPool"));
        assert!(yaml.contains("shortNames"));
        assert!(yaml.contains("fp"));
        assert!(yaml.contains("v1alpha1"));
    }

    /// camelCase renames applied. A missing `#[serde(rename_all)]`
    /// means `kubectl apply` with camelCase YAML silently drops
    /// the field.
    #[test]
    fn camel_case_renames() {
        let crd = FetcherPool::crd();
        let json = serde_json::to_string(&crd).expect("serializes");
        assert!(json.contains("nodeSelector"));
        assert!(json.contains("readyReplicas"));
        assert!(json.contains("desiredReplicas"));
        assert!(json.contains("lastScaleTime"));
        // FetcherSizeClass nested
        assert!(json.contains("maxConcurrent"));
        // Negative: no snake_case leaked as a property KEY.
        assert!(!json.contains("\"node_selector\":"));
        assert!(!json.contains("\"ready_replicas\":"));
        assert!(!json.contains("\"desired_replicas\":"));
        assert!(!json.contains("\"max_concurrent\":"));
    }

    /// `classes` defaults to empty (back-compat: existing FetcherPool
    /// YAMLs without the field deserialize as single-size pools).
    #[test]
    fn classes_default_empty() {
        let yaml = r#"
            maxConcurrent: 8
            autoscaling: {metric: fodQueueDepth, targetValue: 5}
            image: rio-fetcher:test
            systems: [x86_64-linux]
        "#;
        let spec: FetcherPoolSpec = serde_yml::from_str(yaml).expect("deserializes");
        assert!(spec.classes.is_empty(), "absent → empty (back-compat)");
    }

    /// The `classes`/`resources` mutual-exclusion CEL rule renders.
    /// Absence means a misformed spec with BOTH set is accepted at
    /// admission; the reconciler would silently ignore one.
    #[test]
    fn classes_resources_cel_renders() {
        let crd = FetcherPool::crd();
        let json = serde_json::to_string(&crd).expect("serializes");
        assert!(json.contains("size(self.classes) == 0 || !has(self.resources)"));
    }

    /// The `maxConcurrent > 0` CEL rule renders.
    #[test]
    fn max_concurrent_cel_renders() {
        let crd = FetcherPool::crd();
        let json = serde_json::to_string(&crd).expect("serializes");
        assert!(
            json.contains("self.maxConcurrent > 0"),
            "maxConcurrent>0 CEL rule missing from schema"
        );
    }
}
