//! ComponentScaler CRD: predictive replica autoscaling for rio's
//! stateless control-plane Deployments (currently rio-store).
//!
//! The reconciler computes `desired = ceil(builders / learnedRatio)`
//! from the scheduler's `Σ(queued+running)` (predictive — store can
//! scale BEFORE the burst hits), then EMA-corrects `learnedRatio`
//! against `max(GetLoad)` across the target's pods (observed — keeps
//! the prediction honest as rio evolves). See `r[ctrl.scaler.
//! component]` / `r[ctrl.scaler.ratio-learn]` in controller.md.
//!
//! Why not k8s HPA: no metrics-server / custom.metrics.k8s.io adapter
//! in-cluster. The controller already has the demand signal
//! (`GetSizeClassStatus`) and the reconcile-loop pattern; an HPA-on-
//! custom-metrics path would mean deploying that stack first.

use k8s_openapi::apimachinery::pkg::apis::meta::v1::Time;
use kube::{CustomResource, KubeSchema};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::builderpool::Replicas;

/// Predictive demand signal. Enum (not free-form string) because the
/// reconciler hard-branches on it — an unknown signal isn't a
/// degraded mode, it's "the reconciler has nothing to compute from."
/// `SelfReported` is reserved for gateway scaling (load ∝ connected
/// nix clients, not builders); the second CR ships when multi-client
/// load exists to test against.
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq, Eq, JsonSchema)]
#[serde(rename_all = "kebab-case")]
pub enum Signal {
    /// `Σ(class.queued + class.running)` from `AdminService.
    /// GetSizeClassStatus`. Builders are the store's load source
    /// (~12 batch-RPCs/builder post-I-110) so `store_load ∝
    /// builder_count` is the assumption the learned ratio corrects.
    #[default]
    SchedulerBuilders,
    /// Reserved. Gateway load ∝ connected `nix` clients, not
    /// builders; signal architecture differs.
    SelfReported,
}

/// Reference to the scaled workload. Only `Deployment` for now —
/// rio-store is a Deployment (stateless at runtime; PG + S3 hold
/// everything). Kept as a struct rather than a bare name so a
/// future StatefulSet target doesn't need a CRD version bump.
#[derive(Serialize, Deserialize, Clone, Debug, KubeSchema)]
#[serde(rename_all = "camelCase")]
#[x_kube(
    validation = Rule::new("self.kind == 'Deployment'").message(
        "targetRef.kind must be 'Deployment' — ComponentScaler patches the apps/v1 \
         deployments/scale subresource; StatefulSet targets are not yet supported"
    )
)]
pub struct TargetRef {
    /// `Deployment`. CEL-enforced.
    pub kind: String,
    /// `metadata.name` of the Deployment in the SAME namespace as
    /// this ComponentScaler.
    pub name: String,
}

/// Load thresholds for ratio correction. The reconciler compares
/// `max(GetLoad)` against these to decide whether the learned ratio
/// is too high (under-provisioning → load spikes past `high`) or
/// too low (over-provisioning → load idles below `low`).
#[derive(Serialize, Deserialize, Clone, Debug, KubeSchema)]
#[serde(rename_all = "camelCase")]
#[x_kube(
    validation = Rule::new(
        "self.low > 0.0 && self.low < self.high && self.high <= 1.0"
    ).message(
        "loadThresholds must satisfy 0.0 < low < high <= 1.0"
    )
)]
pub struct LoadThresholds {
    /// Above this: immediate `+1` AND `learnedRatio *= 0.95`.
    /// Asymmetric correction — under-provisioning is dangerous
    /// (I-105 cascade). Default 0.8.
    #[serde(default = "default_high")]
    pub high: f64,
    /// Below this for `LOW_LOAD_TICKS_FOR_RATIO_GROWTH` consecutive
    /// ticks: `learnedRatio *= 1.02`. Slow growth — over-provisioning
    /// is cheap. Default 0.3.
    #[serde(default = "default_low")]
    pub low: f64,
}

impl Default for LoadThresholds {
    fn default() -> Self {
        Self {
            high: default_high(),
            low: default_low(),
        }
    }
}

fn default_high() -> f64 {
    0.8
}
fn default_low() -> f64 {
    0.3
}
fn default_seed_ratio() -> f64 {
    50.0
}

/// ComponentScaler spec. The derive generates a `ComponentScaler`
/// struct with `.metadata`, `.spec` (this), `.status`.
///
/// Namespaced: the ComponentScaler lives alongside its target
/// Deployment (rio-store namespace for the store scaler). The
/// reconciler patches `deployments/scale` in the SAME namespace.
///
/// `KubeSchema` alongside `CustomResource`: same pattern as
/// BuilderPoolSpec — KubeSchema processes `#[x_kube]` on nested
/// structs (Replicas, TargetRef, LoadThresholds) so their CEL
/// rules render.
#[derive(CustomResource, Deserialize, Serialize, Clone, Debug, KubeSchema)]
#[kube(
    group = "rio.build",
    version = "v1alpha1",
    kind = "ComponentScaler",
    namespaced,
    status = "ComponentScalerStatus",
    shortname = "cscaler",
    printcolumn = r#"{"name":"Target","type":"string","jsonPath":".spec.targetRef.name"}"#,
    printcolumn = r#"{"name":"Desired","type":"integer","jsonPath":".status.desiredReplicas"}"#,
    printcolumn = r#"{"name":"Ratio","type":"number","jsonPath":".status.learnedRatio"}"#,
    printcolumn = r#"{"name":"Load","type":"number","jsonPath":".status.observedLoadFactor"}"#,
    printcolumn = r#"{"name":"Age","type":"date","jsonPath":".metadata.creationTimestamp"}"#
)]
#[serde(rename_all = "camelCase")]
pub struct ComponentScalerSpec {
    /// The Deployment to scale. Same-namespace.
    pub target_ref: TargetRef,

    /// Predictive demand signal. Default `scheduler-builders` — the
    /// only implemented signal today. `self-reported` is reserved.
    #[serde(default)]
    pub signal: Signal,

    /// Replica bounds. The reconciler clamps to `[min, max]`.
    /// `max` for rio-store should be `Aurora max_connections /
    /// pgMaxConnections` — at 16ACU≈2800 / 200, that's 14. The
    /// `min <= max` CEL rule is inherited from `Replicas`.
    pub replicas: Replicas,

    /// Initial `builders_per_replica` for a fresh CR (no
    /// `.status.learnedRatio` yet). Empirically ~70 from 555-on-8
    /// (I-110); seed at 50 to start slightly over-provisioned (safe
    /// side) and let the ratio EMA up.
    #[serde(default = "default_seed_ratio")]
    pub seed_ratio: f64,

    /// `host:port` of the headless Service whose endpoints are the
    /// target's pods. The reconciler DNS-resolves this and calls
    /// `StoreAdminService.GetLoad` on each IP. Cross-namespace FQDN
    /// (e.g. `rio-store-headless.rio-store:9002`) — the controller
    /// runs in `rio-system`.
    pub load_endpoint: String,

    /// Load thresholds for ratio correction. Defaults `{high: 0.8,
    /// low: 0.3}`.
    #[serde(default)]
    pub load_thresholds: LoadThresholds,
}

/// ComponentScaler status. The reconciler writes ALL fields (single
/// SSA field-manager — there's no separate autoscaler task here, the
/// reconciler IS the autoscaler).
///
/// `learnedRatio` is the durable bit: a controller restart reads
/// this back and continues from where it left off (NOT from
/// `seedRatio`). The other fields are observability.
#[derive(Deserialize, Serialize, Clone, Debug, Default, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ComponentScalerStatus {
    /// EMA-adjusted `builders_per_replica`. Persisted across
    /// controller restarts. Seeded from `spec.seedRatio` on first
    /// reconcile; corrected against `observedLoadFactor` thereafter.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub learned_ratio: Option<f64>,

    /// `max(GetLoad().pg_pool_utilization)` across `loadEndpoint`
    /// pods at the last tick. The "observed" half of the predict-
    /// then-correct loop.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub observed_load_factor: Option<f64>,

    /// What the reconciler last patched onto `deployments/scale`.
    /// May differ from the Deployment's actual `.spec.replicas` if
    /// something else patched it.
    #[serde(default)]
    pub desired_replicas: i32,

    /// When `desired` last increased. The 5-minute scale-down
    /// stabilization window starts here.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(with = "Option<String>")]
    pub last_scale_up_time: Option<Time>,

    /// Consecutive ticks with `observedLoadFactor < loadThresholds.
    /// low`. At `LOW_LOAD_TICKS_FOR_RATIO_GROWTH` (30), the ratio
    /// grows by 2% and this resets. Mirrored to status for
    /// observability; the reconciler's authoritative counter is
    /// in-process (writing it here every tick would self-trigger the
    /// CR watch). Restart resets the streak — at most one extra
    /// 5-minute over-provisioning window per controller restart.
    #[serde(default)]
    pub low_load_ticks: u32,
}

#[cfg(test)]
mod tests {
    use super::*;
    use kube::CustomResourceExt;

    /// CRD serializes without panic. Same smoke check as the other
    /// CRDs — catches schemars/kube derive misconfiguration at
    /// `cargo test` time, not at crdgen run.
    #[test]
    fn crd_serializes() {
        let crd = ComponentScaler::crd();
        let yaml = serde_yml::to_string(&crd).expect("serializes");
        assert!(yaml.contains("group: rio.build"));
        assert!(yaml.contains("kind: ComponentScaler"));
        assert!(yaml.contains("cscaler"));
        assert!(yaml.contains("v1alpha1"));
    }

    /// camelCase renames applied. A missing `#[serde(rename_all)]`
    /// means `kubectl apply` with camelCase YAML silently drops
    /// fields.
    #[test]
    fn camel_case_renames() {
        let crd = ComponentScaler::crd();
        let json = serde_json::to_string(&crd).expect("serializes");
        for k in [
            "targetRef",
            "seedRatio",
            "loadEndpoint",
            "loadThresholds",
            "learnedRatio",
            "observedLoadFactor",
            "desiredReplicas",
            "lastScaleUpTime",
            "lowLoadTicks",
        ] {
            assert!(json.contains(k), "missing camelCase key {k:?}");
        }
        assert!(!json.contains("\"target_ref\""));
        assert!(!json.contains("\"learned_ratio\""));
    }

    /// Nested CEL rules render. `Replicas` (`min <= max`) is
    /// inherited; `TargetRef` and `LoadThresholds` are local.
    #[test]
    fn cel_rules_render() {
        let crd = ComponentScaler::crd();
        let json = serde_json::to_string(&crd).expect("serializes");
        assert!(
            json.contains("self.min <= self.max"),
            "Replicas CEL inherited"
        );
        assert!(
            json.contains("self.kind == 'Deployment'"),
            "TargetRef CEL renders"
        );
        assert!(
            json.contains("self.low < self.high"),
            "LoadThresholds CEL renders"
        );
    }

    /// Signal enum kebab-cases. The CRD spec text and helm template
    /// say `scheduler-builders`; a missing `rename_all` would emit
    /// `SchedulerBuilders` and `kubectl apply` would reject the CR.
    #[test]
    fn signal_kebab_case() {
        let s = serde_json::to_string(&Signal::SchedulerBuilders).unwrap();
        assert_eq!(s, "\"scheduler-builders\"");
        let s = serde_json::to_string(&Signal::SelfReported).unwrap();
        assert_eq!(s, "\"self-reported\"");
        // Default + omitted-field deserialization.
        let yaml = r#"
            targetRef: {kind: Deployment, name: rio-store}
            replicas: {min: 2, max: 14}
            loadEndpoint: rio-store-headless.rio-store:9002
        "#;
        let spec: ComponentScalerSpec = serde_yml::from_str(yaml).expect("deserializes");
        assert_eq!(spec.signal, Signal::SchedulerBuilders);
        assert_eq!(spec.seed_ratio, 50.0);
        assert_eq!(spec.load_thresholds.high, 0.8);
        assert_eq!(spec.load_thresholds.low, 0.3);
    }
}
