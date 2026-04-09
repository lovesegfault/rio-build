//! Shared CRD substructures, embedded via `#[serde(flatten)]`.
//!
//! `BuilderPool` and `FetcherPool` are structurally aligned by design
//! (fetcher tiers mirror builder tiers — same binary, same scheduler,
//! different `RIO_EXECUTOR_KIND`). Factoring the shared fields here
//! encodes that alignment in the type system: a field added to
//! [`PoolSpecCommon`] lands in BOTH CRDs, so the two can't drift.
//!
//! `#[serde(flatten)]` keeps the wire format flat — the OpenAPI schema
//! inlines these properties into the parent (no nested `common: {...}`
//! object). That means existing YAML, the printer-column jsonPaths
//! (`.status.readyReplicas`), and the SSA status-patch bodies in
//! rio-controller are unchanged.
//!
//! Each embedding struct also implements `Deref`/`DerefMut` →
//! `*Common` so call sites keep writing `wp.spec.image` /
//! `pool.status.ready_replicas` instead of `.common.image`. The
//! `impl_common_deref!` macro stamps out those impls.

use std::collections::BTreeMap;

use k8s_openapi::api::core::v1::{ResourceRequirements, Toleration};
use k8s_openapi::apimachinery::pkg::apis::meta::v1::Condition;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// Status fields shared by `BuilderPoolStatus` and `FetcherPoolStatus`.
///
/// `Default`: kube-rs initializes status to `None` → `Some(default())`
/// on first reconcile; all fields zero-value-is-meaningful (0 replicas
/// is a valid observed state).
#[derive(Deserialize, Serialize, Clone, Debug, Default, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct PoolStatusCommon {
    /// Jobs whose pod has passed readinessProbe (heartbeating to
    /// scheduler).
    #[serde(default)]
    pub ready_replicas: i32,
    /// Concurrent-Job target the reconciler is converging on.
    #[serde(default)]
    pub desired_replicas: i32,
    /// Standard K8s Conditions. Both pools currently expose one type:
    /// `SchedulerUnreachable` (status=True when the reconciler's
    /// `ClusterStatus` RPC fails — disambiguates "scheduler idle,
    /// queued=0" from "scheduler down, queued unknown").
    #[serde(default)]
    #[schemars(schema_with = "crate::any_object_array")]
    pub conditions: Vec<Condition>,
}

/// Deployment knobs shared by `PoolSpecCommon` (→ BuilderPool /
/// FetcherPool) AND `PoolTemplate` (→ BuilderPoolSet). Factored
/// because the BPS reconciler's `build_child_builderpool` was
/// copying these field-by-field from template to child spec; with
/// the struct shared, that becomes one `template.deploy.clone()`.
///
/// `Default`: required for `PoolTemplate: Default`. The zero
/// values (`image = ""`, `systems = []`) are NOT valid — the
/// apiserver rejects them via the `required:` schema list — so
/// `Default` is for test fixtures and `..Default::default()` only.
#[derive(Deserialize, Serialize, Clone, Debug, Default, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct PoolDeployKnobs {
    /// Container image ref. Required — there's no sensible default
    /// (depends on how operators build/tag).
    pub image: String,

    /// Target systems (e.g., `["x86_64-linux"]`). Builders AND
    /// fetchers execute derivation `builder` scripts, so the host
    /// system must match.
    pub systems: Vec<String>,

    /// Node selector for the Job pod spec.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub node_selector: Option<BTreeMap<String, String>>,

    /// Tolerations for the Job pod spec. Typed `Toleration` (not
    /// `serde_json::Value`); `any_object_array` passthrough because
    /// k8s-openapi types don't impl JsonSchema.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(schema_with = "crate::any_object_array")]
    pub tolerations: Option<Vec<Toleration>>,

    /// mTLS client cert Secret name (`tls.crt`/`tls.key`/`ca.crt`).
    /// Same cert across builders and fetchers — same binary, same
    /// scheduler/store endpoints. Unset = plaintext gRPC (dev mode).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tls_secret_name: Option<String>,

    /// Explicit `hostUsers` override. `None` defaults to `hostUsers:
    /// false` (userns isolation per ADR-012). Set `true` for k3s/
    /// containerd deployments that don't chown the pod cgroup to the
    /// userns-mapped root UID — see `BuilderPoolSpec.host_users` doc
    /// for the full diagnostic.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub host_users: Option<bool>,
}

/// Spec fields shared by `BuilderPoolSpec` and `FetcherPoolSpec`.
///
/// Fetchers run the SAME `rio-builder` binary with a different
/// `RIO_EXECUTOR_KIND`, so the deployment knobs (image, systems,
/// node placement, mTLS) are identical. Per-role fields (FUSE
/// tuning, seccomp, `resources`, `classes[]`) stay on the outer
/// structs.
#[derive(Deserialize, Serialize, Clone, Debug, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct PoolSpecCommon {
    /// Concurrent-Job ceiling. The reconciler spawns one Job per
    /// dispatch-need up to this many active at once. Both CRDs add
    /// a struct-level CEL `self.maxConcurrent > 0`.
    pub max_concurrent: u32,

    /// Backstop `activeDeadlineSeconds` on Jobs. `None` = per-role
    /// default (3600 for builders, 300 for fetchers).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub deadline_seconds: Option<u32>,

    /// `image` / `systems` / `node_selector` / `tolerations` /
    /// `tls_secret_name` / `host_users`. Flattened — wire format
    /// unchanged; `Deref` keeps `wp.spec.image` working.
    #[serde(flatten)]
    pub deploy: PoolDeployKnobs,
}

/// Fields shared by `SizeClassSpec` (builder) and `FetcherSizeClass`.
///
/// Builder classes additionally carry `cutoff_secs` (a-priori
/// duration routing); fetcher classes are reactive-only and have
/// no extra fields.
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct SizeClassCommon {
    /// Class name. Becomes the child pool / Job name suffix AND the
    /// `RIO_SIZE_CLASS` env the executor reports in its heartbeat.
    pub name: String,

    /// K8s resource requests/limits for this class's pods.
    /// NON-Option: distinct resource profiles are the entire point
    /// of size classes. `any_object` passthrough — see
    /// `crate::any_object` for why.
    #[schemars(schema_with = "crate::any_object")]
    pub resources: ResourceRequirements,

    /// Concurrent-Job ceiling for this class. `None` = inherit
    /// `spec.max_concurrent`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_concurrent: Option<u32>,
}

/// Stamps `Deref`/`DerefMut` from an embedding struct to its
/// `#[serde(flatten)]` field. Lets call sites keep writing
/// `wp.spec.image` (auto-deref through `PoolSpecCommon` →
/// `PoolDeployKnobs`) instead of `wp.spec.common.deploy.image`,
/// so the refactor doesn't churn rio-controller / rio-cli.
///
/// Two-arg form defaults the field name to `common` (the
/// majority case); three-arg form takes an explicit field
/// name (`deploy` for the `PoolDeployKnobs` layer).
///
/// `#[macro_export]` not used: `pub(crate)` semantics via the
/// `pub(crate) use` re-export below — only embedding CRD modules
/// in this crate need it.
macro_rules! impl_common_deref {
    ($outer:ty => $inner:ty) => {
        impl_common_deref!($outer => $inner, common);
    };
    ($outer:ty => $inner:ty, $field:ident) => {
        impl ::core::ops::Deref for $outer {
            type Target = $inner;
            fn deref(&self) -> &Self::Target {
                &self.$field
            }
        }
        impl ::core::ops::DerefMut for $outer {
            fn deref_mut(&mut self) -> &mut Self::Target {
                &mut self.$field
            }
        }
        impl ::core::convert::AsRef<$inner> for $outer {
            fn as_ref(&self) -> &$inner {
                &self.$field
            }
        }
    };
}
pub(crate) use impl_common_deref;

// Chained Deref: `BuilderPoolSpec` → `PoolSpecCommon` →
// `PoolDeployKnobs`, so `wp.spec.image` resolves through both
// layers. Declared after the macro because `macro_rules!`
// visibility is textual-order.
impl_common_deref!(PoolSpecCommon => PoolDeployKnobs, deploy);

#[cfg(test)]
mod tests {
    use kube::CustomResourceExt;

    /// `#[serde(flatten)]` inlines common properties into the parent
    /// schema — there is NO `common: {...}` object anywhere in the
    /// rendered CRDs. Guards the wire-format invariant: existing
    /// YAML, the printer-column jsonPaths (`.status.readyReplicas`),
    /// and rio-controller's SSA status-patch bodies all assume the
    /// flat shape.
    #[test]
    fn flatten_does_not_nest() {
        for crd in [
            crate::builderpool::BuilderPool::crd(),
            crate::fetcherpool::FetcherPool::crd(),
            crate::builderpoolset::BuilderPoolSet::crd(),
        ] {
            let json = serde_json::to_string(&crd).unwrap();
            for field in ["common", "deploy"] {
                assert!(
                    !json.contains(&format!("\"{field}\"")),
                    "{}: flattened `{field}` field leaked into the CRD schema \
                     as a property — schemars/kube-derive flatten regression",
                    crd.spec.names.kind
                );
            }
        }
    }

    /// `required:` lists survive the flatten round-trip. schemars
    /// merges the inner struct's `required` into the parent — a
    /// regression here would mean `image` / `maxConcurrent` /
    /// `systems` silently become optional and `kubectl apply` with
    /// an incomplete spec is accepted. All three required fields
    /// come from PoolSpecCommon (`maxConcurrent` directly, `image`
    /// / `systems` via the nested `PoolDeployKnobs` flatten); the
    /// outer structs contribute none (every builder-/fetcher-only
    /// field is optional or defaulted).
    #[test]
    fn flatten_preserves_required() {
        for crd in [
            crate::builderpool::BuilderPool::crd(),
            crate::fetcherpool::FetcherPool::crd(),
        ] {
            let json = serde_json::to_string(&crd).unwrap();
            assert!(
                json.contains(r#""required":["image","maxConcurrent","systems"]"#),
                "{}: spec.required drifted — flatten dropped or added a \
                 required field",
                crd.spec.names.kind,
            );
        }
        // BuilderPoolSet.spec.poolTemplate gets `image` / `systems`
        // via the same PoolDeployKnobs flatten — verify the second
        // embedding site's required-merge too.
        let json = serde_json::to_string(&crate::builderpoolset::BuilderPoolSet::crd()).unwrap();
        assert!(
            json.contains(r#""required":["image","systems"]"#),
            "BuilderPoolSet: poolTemplate.required drifted — flatten dropped \
             a PoolDeployKnobs required field",
        );
    }
}
