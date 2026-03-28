//! FetcherPool CRD: StatefulSet of rio-builder pods in fetcher mode
//! (`RIO_EXECUTOR_KIND=fetcher`).
//!
//! Fetchers run FOD-only (fixed-output derivations — source tarballs,
//! git clones) with open internet egress. The FOD hash check is the
//! integrity boundary: a tampered fetch produces a hash mismatch that
//! `verify_fod_hashes()` rejects before upload. See ADR-019.
//!
//! SKELETON ONLY (P0451): reconciler wiring, NetworkPolicy, seccomp
//! hardening, scheduler FOD routing are follow-on plans. This file
//! defines the types; nothing reconciles them yet.

use std::collections::BTreeMap;

use k8s_openapi::api::core::v1::{ResourceRequirements, Toleration};
use kube::{CustomResource, KubeSchema};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// FetcherPool spec. Minimal — fetchers are network-bound, not
/// CPU-predictable, so no size-class. The reconciler (follow-on plan)
/// labels pods `rio.build/role: fetcher` and sets stricter
/// `securityContext` (`readOnlyRootFilesystem: true`, stricter seccomp).
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
    printcolumn = r#"{"name":"Desired","type":"integer","jsonPath":".spec.replicas"}"#,
    printcolumn = r#"{"name":"Age","type":"date","jsonPath":".metadata.creationTimestamp"}"#
)]
#[serde(rename_all = "camelCase")]
pub struct FetcherPoolSpec {
    /// Fixed replica count. No autoscaler bounds — fetches are
    /// network-bound and the FOD queue depth is a simpler scaling
    /// signal than duration-EMA. A future plan may add HPA-on-queue.
    pub replicas: i32,

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

    /// `securityContext.readOnlyRootFilesystem` — ADR-019 hardening.
    /// `None` defaults to `true` (the ADR-019 spec). Set `false` for
    /// Bottlerocket where even `hostUsers: true` + rw-remount can't
    /// write `/sys/fs/cgroup` under readOnlyRoot (additional LSM/
    /// containerd hardening). k3s handles readOnlyRoot fine.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub read_only_root_filesystem: Option<bool>,
}

/// FetcherPool status. Reconciler writes (follow-on plan);
/// `kubectl get fp` reads.
#[derive(Deserialize, Serialize, Clone, Debug, Default, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct FetcherPoolStatus {
    /// StatefulSet `.status.readyReplicas`. Passed readinessProbe
    /// = heartbeating to scheduler as `ExecutorKind::Fetcher`.
    #[serde(default)]
    pub ready_replicas: i32,
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
        // Negative: no snake_case leaked as a property KEY.
        assert!(!json.contains("\"node_selector\":"));
        assert!(!json.contains("\"ready_replicas\":"));
    }
}
