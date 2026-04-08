//! FetcherPool CRD: one-shot Jobs of rio-builder in fetcher mode
//! (`RIO_EXECUTOR_KIND=fetcher`).
//!
//! Fetchers run FOD-only (fixed-output derivations — source tarballs,
//! git clones) with open internet egress. The FOD hash check is the
//! integrity boundary: a tampered fetch produces a hash mismatch that
//! `verify_fod_hashes()` rejects before upload. See ADR-019.

use kube::{CustomResource, KubeSchema};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::common::{PoolSpecCommon, PoolStatusCommon, SizeClassCommon, impl_common_deref};

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
// single-size pool uses top-level resources.
#[x_kube(
    validation = Rule::new(
        "size(self.classes) == 0 || !has(self.resources)"
    ).message(
        "classes[] and resources are mutually exclusive — set per-class resources in classes[].resources, or omit classes[] for a single-size pool"
    )
)]
#[serde(rename_all = "camelCase")]
pub struct FetcherPoolSpec {
    /// Fields shared with `BuilderPoolSpec` (image, systems,
    /// max_concurrent, node placement, resources, mTLS, …).
    /// Flattened — the rendered CRD has these as top-level spec
    /// properties. `Deref`/`DerefMut` below let call sites keep
    /// writing `fp.spec.image`.
    ///
    /// Fetcher-side notes on common fields:
    ///   * `deadline_seconds`: default 300 (`FOD_DEADLINE_SECS`) —
    ///     fetches are network-bound and short; a stuck download is
    ///     the failure mode, not a 90min compile.
    ///   * `resources`: mutually exclusive with `classes[]` (CEL-
    ///     enforced) — when `classes` is non-empty, per-class
    ///     resources apply and this field MUST be unset.
    ///   * `node_selector` / `tolerations`: pair with the
    ///     `rio.build/node-role: fetcher` label and
    ///     `rio.build/fetcher=true:NoSchedule` taint on the
    ///     dedicated fetcher NodePool.
    #[serde(flatten)]
    pub common: PoolSpecCommon,

    /// Optional size classes (I-170). When empty (default), single
    /// Job loop at `spec.resources`. When non-empty, the reconciler
    /// runs one Job loop per class; each registers with
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
}

/// One fetcher size class. Mirrors `SizeClassSpec` minus
/// `cutoff_secs` — fetchers route by reactive floor only, not
/// duration estimate. The scheduler's `[[fetcher_size_classes]]`
/// config carries just the names (for ordering); per-class
/// `resources` live here on the controller side.
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct FetcherSizeClass {
    /// `name` / `resources` / `max_concurrent`. Flattened — wire
    /// format unchanged.
    #[serde(flatten)]
    pub common: SizeClassCommon,
}

impl_common_deref!(FetcherSizeClass => SizeClassCommon);

impl From<SizeClassCommon> for FetcherSizeClass {
    fn from(common: SizeClassCommon) -> Self {
        Self { common }
    }
}

/// FetcherPool status. Reconciler writes; `kubectl get fp` reads.
#[derive(Deserialize, Serialize, Clone, Debug, Default, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct FetcherPoolStatus {
    /// `ready_replicas` / `desired_replicas` / `conditions`.
    /// Flattened — `kubectl` columns and the SSA status patch see
    /// `.status.readyReplicas`, not `.status.common.readyReplicas`.
    #[serde(flatten)]
    pub common: PoolStatusCommon,
}

impl_common_deref!(FetcherPoolSpec => PoolSpecCommon);
impl_common_deref!(FetcherPoolStatus => PoolStatusCommon);

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
