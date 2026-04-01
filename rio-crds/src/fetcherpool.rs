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

use crate::builderpool::{Autoscaling, Replicas};

/// FetcherPool spec. Fetchers are network-bound, not CPU-predictable,
/// so no size-class. The reconciler labels pods `rio.build/role:
/// fetcher` and sets stricter `securityContext` (`readOnlyRootFilesystem:
/// true`, stricter seccomp).
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
#[serde(rename_all = "camelCase")]
pub struct FetcherPoolSpec {
    /// Replica bounds. Autoscaler clamps to [min, max] based on
    /// `ClusterStatus.queued_fod_derivations`.
    ///
    /// Same `{min, max}` shape as BuilderPool. FODs spike at build
    /// start (40+ FODs ready simultaneously when chains hit FOD-phase
    /// together) but are short (~seconds each), so the autoscaler
    /// scales up fast (30s window) and back down slow (10m window) —
    /// same stabilization timing as builders.
    ///
    /// **BREAKING** (I-014): was `replicas: i32`. Existing FetcherPool
    /// CRs need `kubectl edit` or re-apply with `{min: N, max: N}` to
    /// pin the prior static count.
    pub replicas: Replicas,

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

    /// K8s resource requests/limits. Fetchers are lighter than
    /// builders (network-bound, not CPU) — typical: 500m/1Gi.
    /// `any_object` passthrough — see builderpool.rs for why.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(schema_with = "crate::any_object")]
    pub resources: Option<ResourceRequirements>,

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
        // Negative: no snake_case leaked as a property KEY.
        assert!(!json.contains("\"node_selector\":"));
        assert!(!json.contains("\"ready_replicas\":"));
        assert!(!json.contains("\"desired_replicas\":"));
    }

    /// Replicas inherits the `min <= max` CEL rule from BuilderPool's
    /// shared struct. Surfaced in the CRD's openAPIV3Schema as an
    /// `x-kubernetes-validations` entry — the apiserver enforces it.
    #[test]
    fn replicas_cel_inherited() {
        let crd = FetcherPool::crd();
        let json = serde_json::to_string(&crd).expect("serializes");
        assert!(
            json.contains("self.min <= self.max"),
            "Replicas CEL rule renders; without it the autoscaler's clamp() panics on inverted bounds"
        );
    }
}
