//! WorkerPool CRD: one StatefulSet of rio-worker pods.
//!
//! The reconciler creates/updates a StatefulSet owned by this CR
//! (ownerReference → GC on delete). Autoscaler patches
//! `StatefulSet.spec.replicas` based on `ClusterStatus.queued_derivations`.

use k8s_openapi::api::core::v1::{ResourceRequirements, Toleration};
use k8s_openapi::apimachinery::pkg::apis::meta::v1::{Condition, Time};
use kube::{CustomResource, KubeSchema};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// Spec for a worker pool. The derive generates a `WorkerPool`
/// struct with `.metadata`, `.spec` (this), `.status`.
///
/// `namespaced` because WorkerPools are per-namespace (multiple
/// tenants can have their own pools). Cluster-scoped would mean
/// one global set — too rigid.
///
/// Printer columns: what `kubectl get workerpools` shows. Ready/
/// Desired at a glance is the main thing operators want.
///
/// `KubeSchema` alongside `CustomResource`: KubeSchema processes
/// `#[x_kube(validation)]` attrs into x-kubernetes-validations.
/// CustomResource alone ignores them — the schema would have no
/// CEL rules and the apiserver would accept invalid specs. The
/// two derives cooperate: CustomResource generates the full CRD
/// wrapper, KubeSchema handles the schema internals (including
/// the JsonSchema impl, so don't derive that separately).
#[derive(CustomResource, Deserialize, Serialize, Clone, Debug, KubeSchema)]
#[kube(
    group = "rio.build",
    version = "v1alpha1",
    kind = "WorkerPool",
    namespaced,
    status = "WorkerPoolStatus",
    shortname = "wp",
    printcolumn = r#"{"name":"Ready","type":"integer","jsonPath":".status.readyReplicas"}"#,
    printcolumn = r#"{"name":"Desired","type":"integer","jsonPath":".status.desiredReplicas"}"#,
    printcolumn = r#"{"name":"Class","type":"string","jsonPath":".spec.sizeClass"}"#,
    printcolumn = r#"{"name":"Age","type":"date","jsonPath":".metadata.creationTimestamp"}"#
)]
#[serde(rename_all = "camelCase")]
pub struct WorkerPoolSpec {
    /// Replica bounds. Autoscaler clamps to [min, max].
    ///
    /// CEL on the struct (not this field) because it's a cross-field
    /// constraint. See `Replicas` below.
    pub replicas: Replicas,

    /// Autoscaling policy. `target_value` is queued-derivations-per-
    /// worker: scale up when `queued / active_workers > target`.
    pub autoscaling: Autoscaling,

    /// K8s resource requests/limits for the worker container.
    /// `None` = unbounded (cluster default). Operators should set
    /// this — unbounded workers on a shared node is a noisy-
    /// neighbor risk.
    ///
    /// schemars(schema_with): k8s-openapi types don't impl
    /// JsonSchema. `any_object` emits `type: object` +
    /// `x-kubernetes-preserve-unknown-fields: true` — the
    /// apiserver validates against its OWN schema (it knows
    /// ResourceRequirements), we just tell it "object, don't
    /// strip unknowns." `serde_json::Value` emitted `{}` which
    /// the apiserver REJECTS (`type: Required value`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(schema_with = "crate::crds::any_object")]
    pub resources: Option<ResourceRequirements>,

    /// Maximum concurrent builds per worker pod. Maps to
    /// `RIO_MAX_BUILDS` env var. CEL: must be >= 1 (0 builds =
    /// pointless worker).
    #[x_kube(validation = "self >= 1")]
    pub max_concurrent_builds: i32,

    /// FUSE cache size as a K8s Quantity string (e.g., "100Gi").
    /// Maps to an emptyDir sizeLimit + parsed to bytes for
    /// `RIO_FUSE_CACHE_SIZE_GB` env. String because Quantity is
    /// standard K8s (operators know "50Gi" syntax); the reconciler
    /// parses it.
    ///
    /// Why not u64 bytes directly: operators write "100Gi" in
    /// kustomize; making them write 107374182400 is hostile.
    #[serde(default = "default_fuse_cache_size")]
    pub fuse_cache_size: String,

    /// requiredSystemFeatures this pool advertises (e.g., "kvm",
    /// "big-parallel"). Worker's Nix config `system-features`.
    #[serde(default)]
    pub features: Vec<String>,

    /// Target systems (e.g., "x86_64-linux"). CEL: non-empty —
    /// a worker that builds nothing is a config error.
    #[x_kube(validation = "size(self) > 0")]
    pub systems: Vec<String>,

    /// Size class name. Maps to `RIO_SIZE_CLASS` env. Scheduler
    /// routes by this (classify() → matching-class workers).
    /// Empty = unclassified (scheduler with size_classes
    /// configured REJECTS unclassified workers — visible
    /// misconfig failure).
    ///
    /// Convention: pool name = size class name. Not enforced
    /// (you might have `small-x86` and `small-arm` pools both
    /// size_class="small").
    #[serde(default)]
    pub size_class: String,

    /// Container image ref. Required — there's no sensible
    /// default (depends on how operators build/tag).
    pub image: String,

    /// Container imagePullPolicy. None = K8s default (IfNotPresent
    /// for tagged images, Always for `:latest`). Airgap/dev clusters
    /// (k3s/kind with `ctr images import`) MUST set "IfNotPresent"
    /// or "Never" — `:latest` otherwise tries docker.io and fails.
    ///
    /// Controller-managed StatefulSets can't be kustomize-patched
    /// (the CRD is patched, not the generated STS), so this has to
    /// be a CRD field.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub image_pull_policy: Option<String>,

    /// Node selector for the StatefulSet pod spec. Common:
    /// `rio.build/worker: "true"` to confine to tainted nodes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub node_selector: Option<BTreeMap<String, String>>,

    /// Tolerations for the StatefulSet pod spec. Pairs with
    /// node_selector: tolerate the `rio.build/worker:NoSchedule`
    /// taint so workers (and only workers) land on those nodes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(schema_with = "crate::crds::any_object_array")]
    pub tolerations: Option<Vec<Toleration>>,

    /// Run the worker container privileged. None/false = the
    /// default granular caps (SYS_ADMIN + SYS_CHROOT), which is
    /// sufficient on most clusters. true = full privileged,
    /// which disables seccomp and grants ALL caps.
    ///
    /// When to set true: k3s/kind often have containerd seccomp
    /// profiles that block mount(2) even with SYS_ADMIN.
    /// Production on EKS/GKE with proper runtime config should
    /// NOT need this.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub privileged: Option<bool>,

    /// Use the node's network namespace (`hostNetwork: true`).
    /// None/false = pod has its own netns (the default, CNI-
    /// assigned IP). true = pod shares the node's IP + DNS +
    /// /etc/hosts.
    ///
    /// When to set true: VM tests where scheduler/store run on
    /// a separate VM reachable by node hostname but not cluster
    /// DNS; or bare-metal where pod networking adds unwanted
    /// latency (worker → store is NAR-heavy).
    ///
    /// Caveat: hostNetwork pods can't use containerPort-based
    /// Services. The worker doesn't serve anything inbound
    /// (metrics/health are scraped from the node), so this
    /// doesn't break it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub host_network: Option<bool>,
}

/// Replica bounds with cross-field CEL.
///
/// `KubeSchema` (NOT `JsonSchema`) — the kube-rs derive that
/// processes `#[x_kube(validation)]` attributes and emits
/// x-kubernetes-validations into the generated schema. It also
/// implements JsonSchema internally (the two conflict if both
/// derived). CustomResource auto-processes kube attrs on the
/// top-level Spec; nested structs need KubeSchema explicitly.
///
/// Why min/max instead of a single replicas field: autoscaler
/// needs BOUNDS, not a fixed number. Operator says "2-20"; the
/// autoscaler picks within that based on queue depth.
#[derive(Deserialize, Serialize, Clone, Debug, KubeSchema)]
#[serde(rename_all = "camelCase")]
#[x_kube(validation = "self.min <= self.max")]
pub struct Replicas {
    /// Floor. Autoscaler never scales below this, even with empty
    /// queue. Keeps a warm pool for fast dispatch when builds
    /// arrive (cold start = minutes of pod scheduling + FUSE warm).
    pub min: i32,
    /// Ceiling. Cost control — don't burn through the cluster
    /// under a pathological burst.
    pub max: i32,
}

#[derive(Deserialize, Serialize, Clone, Debug, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct Autoscaling {
    /// Metric driving scale decisions. Currently only "queueDepth"
    /// (scheduler's ClusterStatus.queued_derivations). String not
    /// enum so future metrics don't need a CRD version bump —
    /// unknown metric is a RECONCILE error (surfaces in
    /// .status.conditions), not a schema rejection.
    #[serde(default = "default_metric")]
    pub metric: String,
    /// Scale up when `queued_derivations / active_workers >
    /// target_value`. "5" means "scale up when there are more
    /// than 5 queued builds per worker." Lower = more aggressive
    /// scaling (more pods, lower queue latency, higher cost).
    #[serde(default = "default_target")]
    pub target_value: i32,
}

/// WorkerPool status — reconciler writes, operators read.
///
/// `Default` because kube-rs initializes it to `None` → `Some(default())`
/// on first reconcile. All fields zero-value-is-meaningful (0 replicas
/// is a valid observed state).
#[derive(Deserialize, Serialize, Clone, Debug, Default, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct WorkerPoolStatus {
    /// Observed StatefulSet.status.replicas. What's actually
    /// running (may lag desired during rollout).
    #[serde(default)]
    pub replicas: i32,
    /// Observed StatefulSet.status.readyReplicas. Passed
    /// readinessProbe = heartbeat accepted.
    #[serde(default)]
    pub ready_replicas: i32,
    /// What the autoscaler WANTS. Clamped to [min, max]. May
    /// differ from StatefulSet.spec.replicas during the
    /// stabilization window (we want 8 but haven't patched yet).
    #[serde(default)]
    pub desired_replicas: i32,
    /// Last time replicas was actually patched (for stabilization
    /// window computation). kube 3.0: Time = jiff::Timestamp
    /// wrapper, not chrono.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(with = "Option<String>")]
    pub last_scale_time: Option<Time>,
    /// Standard K8s Conditions. Single `Scaling` type with reason
    /// ScaledUp / ScaledDown / UnknownMetric (status=False for
    /// errors). Helps operators see WHY replicas is what it is
    /// ("ScaledUp" with a message showing from/to; "UnknownMetric"
    /// when spec.autoscaling.metric is unsupported).
    #[serde(default)]
    #[schemars(schema_with = "crate::crds::any_object_array")]
    pub conditions: Vec<Condition>,
}

// ----- serde defaults --------------------------------------------------------
// Functions because serde default needs fn() -> T, not const.

fn default_fuse_cache_size() -> String {
    "50Gi".into()
}

fn default_metric() -> String {
    "queueDepth".into()
}

fn default_target() -> i32 {
    5
}

#[cfg(test)]
mod tests {
    use super::*;
    use kube::CustomResourceExt;

    /// The CRD serializes without panic. Catches:
    /// - schemars derive blowing up on a weird type
    /// - kube's CustomResourceExt::crd() not handling our #[kube]
    ///   attrs (e.g., printcolumn with bad JSON)
    ///
    /// Doesn't validate the OpenAPI schema is CORRECT — K8s
    /// apiserver does that. Just "it produces output."
    #[test]
    fn crd_serializes() {
        let crd = WorkerPool::crd();
        let yaml = serde_yml::to_string(&crd).expect("serializes");
        // Smoke check: the group/kind we configured are in there.
        assert!(yaml.contains("group: rio.build"));
        assert!(yaml.contains("kind: WorkerPool"));
        assert!(yaml.contains("shortNames"));
        assert!(yaml.contains("wp"));
    }

    /// CEL validation rules are present in the generated schema.
    /// kube-rs emits them under `x-kubernetes-validations` in
    /// the relevant schema property. Test for the literal rule
    /// strings — a #[x_kube(validation)] attribute silently
    /// dropped (typo in the attr name, wrong derive) would pass
    /// compilation but produce a schema without the constraint.
    /// The apiserver would then accept `min=10, max=5`.
    #[test]
    fn cel_rules_in_schema() {
        let crd = WorkerPool::crd();
        let json = serde_json::to_string(&crd).expect("serializes to JSON");
        // The three #[x_kube(validation)] rules, verbatim.
        assert!(
            json.contains("self.min <= self.max"),
            "Replicas min<=max CEL rule missing from schema"
        );
        assert!(
            json.contains("self >= 1"),
            "max_concurrent_builds >= 1 CEL rule missing"
        );
        assert!(
            json.contains("size(self) > 0"),
            "systems non-empty CEL rule missing"
        );
    }

    /// camelCase field renames applied. The K8s convention is
    /// camelCase in JSON/YAML; Rust is snake_case. serde rename_all
    /// bridges. If someone removes #[serde(rename_all)] from a
    /// nested struct, the schema has `max_concurrent_builds`
    /// instead of `maxConcurrentBuilds` — `kubectl apply` with
    /// camelCase YAML would silently ignore the field (K8s
    /// doesn't error on unknown fields by default).
    #[test]
    fn camel_case_renames() {
        let crd = WorkerPool::crd();
        let json = serde_json::to_string(&crd).expect("serializes");
        assert!(
            json.contains("maxConcurrentBuilds"),
            "camelCase rename missing — kubectl with camelCase YAML would \
             silently ignore the field"
        );
        assert!(!json.contains("max_concurrent_builds"));
        assert!(json.contains("fuseCacheSize"));
        assert!(json.contains("targetValue"));
    }
}
