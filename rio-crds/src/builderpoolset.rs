//! BuilderPoolSet CRD: multi-class BuilderPool orchestration.
//!
//! A BuilderPoolSet owns one child BuilderPool per `SizeClassSpec`.
//! Operators declare size classes ("small", "medium", "large") with
//! cutoff thresholds; the BPS controller (P0233) reconciles child
//! BuilderPools named `{bps}-{class.name}` and keeps their specs in
//! sync with the template. The cutoff rebalancer (P0234) adjusts
//! `effective_cutoff_secs` in status based on observed build-time
//! distributions.
//!
//! This file is type-definitions only — no reconcile logic.

use std::collections::BTreeMap;

use k8s_openapi::api::core::v1::Toleration;
use kube::{CustomResource, KubeSchema};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::builderpool::SeccompProfileKind;
use crate::common::{SizeClassCommon, impl_common_deref};

/// BuilderPoolSet spec. Each `classes[]` entry becomes a child
/// BuilderPool owned by this CR (ownerReference → cascade delete).
///
/// `KubeSchema` alongside `CustomResource`: same pattern as
/// BuilderPoolSpec. No CEL on this struct today, but KubeSchema
/// keeps the door open without a re-derive (CustomResource +
/// JsonSchema conflict if you later add `#[x_kube(validation)]`).
#[derive(CustomResource, Deserialize, Serialize, Clone, Debug, KubeSchema)]
#[kube(
    group = "rio.build",
    version = "v1alpha1",
    kind = "BuilderPoolSet",
    namespaced,
    status = "BuilderPoolSetStatus",
    shortname = "bps",
    printcolumn = r#"{"name":"Classes","type":"string","jsonPath":".spec.classes[*].name"}"#,
    printcolumn = r#"{"name":"Age","type":"date","jsonPath":".metadata.creationTimestamp"}"#
)]
#[serde(rename_all = "camelCase")]
pub struct BuilderPoolSetSpec {
    /// Size classes. Each becomes a child BuilderPool named
    /// `{bps-name}-{class.name}`. Order doesn't matter — the
    /// reconciler keys by `.name`, not position (spec-order
    /// churn shouldn't trigger reconciles).
    pub classes: Vec<SizeClassSpec>,

    /// Template merged into each child BuilderPool's spec. Per-
    /// class fields (resources, cutoff) override; template fields
    /// (image, node_selector, seccomp) apply uniformly.
    pub pool_template: PoolTemplate,
}

/// One size class. The scheduler's classify() maps a derivation's
/// predicted build time to the class whose cutoff is closest-above.
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct SizeClassSpec {
    /// `name` / `resources` / `max_concurrent` (shared with
    /// `FetcherSizeClass`). Flattened — wire format unchanged.
    #[serde(flatten)]
    pub common: SizeClassCommon,

    /// Upper bound (seconds) for builds this class handles. A
    /// build with predicted duration < cutoff_secs routes here.
    /// f64 because build-time predictions are fractional (EMA
    /// output). The LAST class's cutoff is effectively infinity
    /// (it catches everything above the previous cutoff).
    pub cutoff_secs: f64,
}

impl_common_deref!(SizeClassSpec => SizeClassCommon);

/// Subset of `BuilderPoolSpec` shared across all child pools.
/// The reconciler merges this with per-class fields when
/// building child `BuilderPoolSpec`s.
///
/// NOT the full BuilderPoolSpec — only the fields that make
/// sense to share across size classes.
#[derive(Clone, Debug, Default, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct PoolTemplate {
    /// Container image. Shared across classes — same builder
    /// binary, different resource allocations. REQUIRED —
    /// `BuilderPoolSpec.image` has no default. The reconciler's
    /// child builder errors with `InvalidSpec` if this is empty.
    pub image: String,

    /// Target systems (e.g., `["x86_64-linux"]`). Shared across
    /// classes — all size classes in one BPS run the same binary
    /// on the same arch; separate arches warrant separate BPSes.
    /// REQUIRED — child BuilderPool CEL rejects empty `systems[]`.
    pub systems: Vec<String>,

    /// requiredSystemFeatures this pool advertises. Shared
    /// across classes for the same reason as `systems`.
    #[serde(default)]
    pub features: Vec<String>,

    /// Node selector. Shared because builder nodes are usually
    /// tainted/labeled uniformly (`rio.build/builder: "true"`),
    /// not per-class.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub node_selector: Option<BTreeMap<String, String>>,

    /// Tolerations. Shared — pairs with node_selector.
    /// Typed `Toleration` (not serde_json::Value) to match
    /// builderpool.rs; `any_object_array` passthrough because
    /// k8s-openapi types don't impl JsonSchema.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(schema_with = "crate::any_object_array")]
    pub tolerations: Option<Vec<Toleration>>,

    /// Seccomp profile (P0223). Applied uniformly — the syscall
    /// filter doesn't vary by builder size. The Localhost profile
    /// (`infra/helm/rio-build/files/seccomp-rio-builder.json`)
    /// denies ptrace/bpf/setns/process_vm_* regardless of how
    /// big the builder is.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub seccomp_profile: Option<SeccompProfileKind>,

    /// Privileged escape hatch. Shared — cluster runtime
    /// constraints (containerd seccomp blocking mount(2)) are
    /// the same for all class pods on a given node pool.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub privileged: Option<bool>,

    /// Host network. Shared — if one class needs hostNetwork
    /// (e.g., bare-metal with NAR-heavy store traffic), all do.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub host_network: Option<bool>,

    /// Explicit hostUsers override. Shared — cluster runtime cgroup
    /// delegation behavior (containerd OwnerUID under userns) is
    /// the same for all class pods. See `BuilderPoolSpec.host_users`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub host_users: Option<bool>,

    /// mTLS client cert Secret name. Shared — same cert-manager
    /// Certificate across all builder pods regardless of size.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tls_secret_name: Option<String>,
    // fod_proxy_url removed per ADR-019: builders are airgapped; FODs
    // route to FetcherPools which have direct egress. Squid is gone.
}

/// BuilderPoolSet status. Reconciler writes; `kubectl get bps`
/// reads.
#[derive(Clone, Debug, Default, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct BuilderPoolSetStatus {
    /// Per-class observed state. One entry per `spec.classes[]`,
    /// matched by `.name`. A missing entry = reconciler hasn't
    /// processed that class yet (transient) or the child
    /// BuilderPool create failed (check .conditions on the BPS
    /// — P0233 adds those).
    #[serde(default)]
    pub classes: Vec<ClassStatus>,
}

/// One class's observed state. Mirrors the child BuilderPool's
/// status plus BPS-level derived fields (effective cutoff).
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ClassStatus {
    /// Matches `spec.classes[].name`.
    pub name: String,

    /// The cutoff the scheduler ACTUALLY uses right now. Equals
    /// `spec.classes[].cutoff_secs` when learning is off; drifts
    /// toward observed build-time percentiles when on. This (not
    /// the spec value) is what the scheduler's classify() reads.
    pub effective_cutoff_secs: f64,

    /// Builds currently queued for this class. From scheduler
    /// `ClusterStatus.queued_derivations` filtered by size_class.
    pub queued: u64,

    /// Name of the owned child BuilderPool (`{bps}-{name}`).
    /// Stored explicitly (not just derivable) so `kubectl get
    /// bps -o yaml` shows the link without operators having to
    /// know the naming convention.
    pub child_pool: String,

    /// Child BuilderPool's `.status.replicas` (observed). May
    /// lag `ready_replicas` during rollout.
    pub replicas: i32,

    /// Child BuilderPool's `.status.readyReplicas`. Passed
    /// readinessProbe = heartbeating to scheduler. THIS is
    /// the operator signal — Ready/Desired gap diagnoses
    /// stuck rollouts (matches builderpool.rs printcolumns).
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
        let crd = BuilderPoolSet::crd();
        let yaml = serde_yml::to_string(&crd).expect("serializes");
        assert!(yaml.contains("group: rio.build"));
        assert!(yaml.contains("kind: BuilderPoolSet"));
        assert!(yaml.contains("shortNames"));
        assert!(yaml.contains("bps"));
        assert!(yaml.contains("v1alpha1"));
    }

    /// camelCase renames applied across all nested structs.
    /// A missing `#[serde(rename_all)]` on any nested type
    /// means `kubectl apply` with camelCase YAML silently drops
    /// that field (K8s default: unknown fields are discarded).
    #[test]
    fn camel_case_renames() {
        let crd = BuilderPoolSet::crd();
        let json = serde_json::to_string(&crd).expect("serializes");
        // BuilderPoolSetSpec
        assert!(json.contains("poolTemplate"));
        // SizeClassSpec
        assert!(json.contains("cutoffSecs"));
        assert!(json.contains("maxConcurrent"));
        // PoolTemplate
        assert!(json.contains("nodeSelector"));
        assert!(json.contains("seccompProfile"));
        assert!(json.contains("hostNetwork"));
        assert!(json.contains("hostUsers"));
        assert!(json.contains("tlsSecretName"));
        // ClassStatus
        assert!(json.contains("effectiveCutoffSecs"));
        assert!(json.contains("childPool"));
        assert!(json.contains("readyReplicas"));
        // Negative: no snake_case leaked as a property KEY.
        // `"name":` syntax matches JSON object keys only — doc-
        // comment descriptions (which schemars captures and which
        // legitimately mention snake_case in prose) don't match.
        assert!(!json.contains("\"pool_template\":"));
        assert!(!json.contains("\"cutoff_secs\":"));
        assert!(!json.contains("\"seccomp_profile\":"));
        assert!(!json.contains("\"ready_replicas\":"));
    }

    /// PoolTemplate pulls SeccompProfileKind from builderpool.rs
    /// (P0223 entry criterion). The type has `#[x_kube]` CEL
    /// rules — verify those propagate into the BPS schema too
    /// (nested KubeSchema types carry their rules through).
    #[test]
    fn seccomp_cel_propagates() {
        let crd = BuilderPoolSet::crd();
        let json = serde_json::to_string(&crd).expect("serializes");
        // SeccompProfileKind's two CEL rules from builderpool.rs:291-292.
        assert!(
            json.contains("self.type in ['RuntimeDefault', 'Localhost', 'Unconfined']"),
            "SeccompProfileKind type-enum CEL rule missing — \
             PoolTemplate.seccomp_profile schema dropped the nested \
             validation"
        );
        assert!(
            json.contains(
                "self.type == 'Localhost' ? has(self.localhostProfile) : !has(self.localhostProfile)"
            ),
            "SeccompProfileKind localhost-coupling CEL rule missing"
        );
    }
}
