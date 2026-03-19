//! WorkerPoolSet CRD: multi-class WorkerPool orchestration.
//!
//! A WorkerPoolSet owns one child WorkerPool per `SizeClassSpec`.
//! Operators declare size classes ("small", "medium", "large") with
//! cutoff thresholds; the WPS controller (P0233) reconciles child
//! WorkerPools named `{wps}-{class.name}` and keeps their specs in
//! sync with the template. The cutoff rebalancer (P0234) adjusts
//! `effective_cutoff_secs` in status based on observed build-time
//! distributions.
//!
//! This file is type-definitions only — no reconcile logic.

use std::collections::BTreeMap;

use k8s_openapi::api::core::v1::{ResourceRequirements, Toleration};
use kube::{CustomResource, KubeSchema};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::workerpool::SeccompProfileKind;

/// WorkerPoolSet spec. Each `classes[]` entry becomes a child
/// WorkerPool owned by this CR (ownerReference → cascade delete).
///
/// `KubeSchema` alongside `CustomResource`: same pattern as
/// WorkerPoolSpec. No CEL on this struct today, but KubeSchema
/// keeps the door open without a re-derive (CustomResource +
/// JsonSchema conflict if you later add `#[x_kube(validation)]`).
#[derive(CustomResource, Deserialize, Serialize, Clone, Debug, KubeSchema)]
#[kube(
    group = "rio.build",
    version = "v1alpha1",
    kind = "WorkerPoolSet",
    namespaced,
    status = "WorkerPoolSetStatus",
    shortname = "wps",
    printcolumn = r#"{"name":"Classes","type":"integer","jsonPath":".spec.classes[*].name"}"#,
    printcolumn = r#"{"name":"Age","type":"date","jsonPath":".metadata.creationTimestamp"}"#
)]
#[serde(rename_all = "camelCase")]
pub struct WorkerPoolSetSpec {
    /// Size classes. Each becomes a child WorkerPool named
    /// `{wps-name}-{class.name}`. Order doesn't matter — the
    /// reconciler keys by `.name`, not position (spec-order
    /// churn shouldn't trigger reconciles).
    pub classes: Vec<SizeClassSpec>,

    /// Template merged into each child WorkerPool's spec. Per-
    /// class fields (resources, cutoff) override; template fields
    /// (image, node_selector, seccomp) apply uniformly.
    pub pool_template: PoolTemplate,

    /// Cutoff learning config. `None` = static cutoffs (whatever
    /// `classes[].cutoff_secs` says). `Some(..)` enables the EMA
    /// rebalancer — it watches build-time histograms and shifts
    /// `status.classes[].effective_cutoff_secs` to balance queue
    /// load across classes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cutoff_learning: Option<CutoffLearningConfig>,
}

/// One size class. The scheduler's classify() maps a derivation's
/// predicted build time to the class whose cutoff is closest-above.
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct SizeClassSpec {
    /// Class name. Becomes the child WorkerPool's name suffix
    /// (`{wps}-{name}`) AND the `WorkerPoolSpec.size_class` value
    /// the scheduler matches against. Convention: "small" / "medium"
    /// / "large"; nothing enforces that.
    pub name: String,

    /// Upper bound (seconds) for builds this class handles. A
    /// build with predicted duration < cutoff_secs routes here.
    /// f64 because build-time predictions are fractional (EMA
    /// output). The LAST class's cutoff is effectively infinity
    /// (it catches everything above the previous cutoff).
    pub cutoff_secs: f64,

    /// Replica floor for this class's child WorkerPool. `None`
    /// = inherit from `pool_template` or fall through to child
    /// WorkerPool's own default. Per-class because "small" wants
    /// many warm replicas (low-latency dispatch) while "large"
    /// tolerates scale-from-zero (builds are hours; +2min spinup
    /// is noise).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub min_replicas: Option<i32>,

    /// Replica ceiling. `None` = inherit. Per-class because
    /// "large" workers have big resource requests — capping them
    /// prevents one class from starving the cluster.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_replicas: Option<i32>,

    /// Queued-builds-per-replica target for this class's
    /// autoscaler. `compute_desired` (P0234) scales to
    /// `queued / target`. Default 5 (one replica per 5 queued).
    /// Lower = more aggressive scaling = lower queue latency
    /// = higher pod churn. "small" class probably wants lower
    /// (fast builds → operators notice queue lag); "large"
    /// tolerates higher (hours-long builds don't care about
    /// a few extra minutes queued).
    #[serde(
        default = "default_target_queue",
        skip_serializing_if = "Option::is_none"
    )]
    pub target_queue_per_replica: Option<u32>,

    /// K8s resource requests/limits for this class's worker
    /// pods. NON-Option: the entire POINT of size classes is
    /// distinct resource profiles ("small" = 1cpu/2Gi; "large"
    /// = 16cpu/64Gi). An inherited default defeats the purpose.
    /// P0233's child-builder does `Some(class.resources.clone())`
    /// straight into `Option<ResourceRequirements>`.
    ///
    /// `any_object` passthrough — see workerpool.rs for why.
    /// The helper emits `nullable: true` (it was written for
    /// Option fields); here the field is non-Option so serde
    /// rejects `null` at deserialize regardless. Apiserver
    /// accepts `null` → controller fails deserialize with a
    /// clear error — acceptable (operators see it).
    #[schemars(schema_with = "crate::any_object")]
    pub resources: ResourceRequirements,
}

fn default_target_queue() -> Option<u32> {
    Some(5)
}

/// Subset of `WorkerPoolSpec` shared across all child pools.
/// The reconciler merges this with per-class fields when
/// building child `WorkerPoolSpec`s.
///
/// NOT the full WorkerPoolSpec — only the fields that make
/// sense to share across size classes. `max_concurrent_builds`,
/// `fuse_cache_size`, etc. deliberately omitted: those scale
/// WITH class size (a "large" worker should have a bigger FUSE
/// cache). A future plan can add them if the use case emerges.
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct PoolTemplate {
    /// Container image. Shared across classes — same worker
    /// binary, different resource allocations. `None` here
    /// means the reconciler errors (WorkerPoolSpec.image is
    /// required); Option so a future per-class override is
    /// possible.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub image: Option<String>,

    /// Node selector. Shared because worker nodes are usually
    /// tainted/labeled uniformly (`rio.build/worker: "true"`),
    /// not per-class.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub node_selector: Option<BTreeMap<String, String>>,

    /// Tolerations. Shared — pairs with node_selector.
    /// Typed `Toleration` (not serde_json::Value) to match
    /// workerpool.rs; `any_object_array` passthrough because
    /// k8s-openapi types don't impl JsonSchema.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(schema_with = "crate::any_object_array")]
    pub tolerations: Option<Vec<Toleration>>,

    /// Seccomp profile (P0223). Applied uniformly — the syscall
    /// filter doesn't vary by worker size. The Localhost profile
    /// (`infra/helm/rio-build/files/seccomp-rio-worker.json`)
    /// denies ptrace/bpf/setns/process_vm_* regardless of how
    /// big the worker is.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub seccomp_profile: Option<SeccompProfileKind>,
}

/// Cutoff rebalancer config. The controller observes per-class
/// build-time histograms (from scheduler metrics) and shifts
/// effective cutoffs toward the actual distribution.
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct CutoffLearningConfig {
    /// Master switch. `false` = `effective_cutoff_secs` stays
    /// pinned to `spec.classes[].cutoff_secs`. Lets operators
    /// keep the config present (for quick re-enable) without
    /// the rebalancer acting.
    pub enabled: bool,

    /// Minimum observed builds before the rebalancer acts.
    /// Below this, the sample is too small — the EMA would
    /// chase noise. 100 is a rough "one busy hour" default;
    /// cold clusters should raise this.
    #[serde(default = "default_min_samples")]
    pub min_samples: u64,

    /// EMA smoothing factor. `new = alpha*obs + (1-alpha)*old`.
    /// 0.3 means ~3 observation cycles to converge on a new
    /// distribution (forgets old data in ~10 cycles). Lower =
    /// smoother but slower to adapt; higher = responsive but
    /// can oscillate on bursty workloads.
    #[serde(default = "default_ema_alpha")]
    pub ema_alpha: f64,
}

fn default_min_samples() -> u64 {
    100
}

fn default_ema_alpha() -> f64 {
    0.3
}

/// WorkerPoolSet status. Reconciler writes; `kubectl get wps`
/// reads.
#[derive(Clone, Debug, Default, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct WorkerPoolSetStatus {
    /// Per-class observed state. One entry per `spec.classes[]`,
    /// matched by `.name`. A missing entry = reconciler hasn't
    /// processed that class yet (transient) or the child
    /// WorkerPool create failed (check .conditions on the WPS
    /// — P0233 adds those).
    #[serde(default)]
    pub classes: Vec<ClassStatus>,
}

/// One class's observed state. Mirrors the child WorkerPool's
/// status plus WPS-level derived fields (effective cutoff).
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
    /// The autoscaler input — `queued / target_queue_per_replica`
    /// is the desired-replica formula.
    pub queued: u64,

    /// Name of the owned child WorkerPool (`{wps}-{name}`).
    /// Stored explicitly (not just derivable) so `kubectl get
    /// wps -o yaml` shows the link without operators having to
    /// know the naming convention.
    pub child_pool: String,

    /// Child WorkerPool's `.status.replicas` (observed). May
    /// lag `ready_replicas` during rollout.
    pub replicas: i32,

    /// Child WorkerPool's `.status.readyReplicas`. Passed
    /// readinessProbe = heartbeating to scheduler. THIS is
    /// the operator signal — Ready/Desired gap diagnoses
    /// stuck rollouts (matches workerpool.rs printcolumns).
    pub ready_replicas: i32,
}

#[cfg(test)]
mod tests {
    use super::*;
    use kube::CustomResourceExt;

    /// The CRD serializes without panic. Same smoke check as
    /// workerpool.rs — catches schemars derive or #[kube] attr
    /// misconfiguration at `cargo test` time, not at crdgen run.
    #[test]
    fn crd_serializes() {
        let crd = WorkerPoolSet::crd();
        let yaml = serde_yml::to_string(&crd).expect("serializes");
        assert!(yaml.contains("group: rio.build"));
        assert!(yaml.contains("kind: WorkerPoolSet"));
        assert!(yaml.contains("shortNames"));
        assert!(yaml.contains("wps"));
        assert!(yaml.contains("v1alpha1"));
    }

    /// camelCase renames applied across all nested structs.
    /// A missing `#[serde(rename_all)]` on any nested type
    /// means `kubectl apply` with camelCase YAML silently drops
    /// that field (K8s default: unknown fields are discarded).
    #[test]
    fn camel_case_renames() {
        let crd = WorkerPoolSet::crd();
        let json = serde_json::to_string(&crd).expect("serializes");
        // WorkerPoolSetSpec
        assert!(json.contains("poolTemplate"));
        assert!(json.contains("cutoffLearning"));
        // SizeClassSpec
        assert!(json.contains("cutoffSecs"));
        assert!(json.contains("minReplicas"));
        assert!(json.contains("maxReplicas"));
        assert!(json.contains("targetQueuePerReplica"));
        // PoolTemplate
        assert!(json.contains("nodeSelector"));
        assert!(json.contains("seccompProfile"));
        // CutoffLearningConfig
        assert!(json.contains("minSamples"));
        assert!(json.contains("emaAlpha"));
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

    /// PoolTemplate pulls SeccompProfileKind from workerpool.rs
    /// (P0223 entry criterion). The type has `#[x_kube]` CEL
    /// rules — verify those propagate into the WPS schema too
    /// (nested KubeSchema types carry their rules through).
    #[test]
    fn seccomp_cel_propagates() {
        let crd = WorkerPoolSet::crd();
        let json = serde_json::to_string(&crd).expect("serializes");
        // SeccompProfileKind's two CEL rules from workerpool.rs:291-292.
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
