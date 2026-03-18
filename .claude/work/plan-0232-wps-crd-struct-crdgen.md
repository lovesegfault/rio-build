# Plan 0232: WorkerPoolSet CRD struct + crdgen wire

phase4c.md:25-27 — the `WorkerPoolSet` CRD types. This is struct definitions + crdgen registration — NO reconciler (P0233), NO status refresh (P0234), NO CRD yaml commit (P0235). Pure Rust types that `kube-derive` turns into a CRD schema.

**`PoolTemplate` consciously inherits `seccomp_profile`** from [P0223](plan-0223-seccomp-localhost-profile.md). The dep is `type:[P0223]` — the `SeccompProfileKind` enum must exist before `PoolTemplate` can reference it. If P0223 is unmerged at dispatch, `PoolTemplate` gets a `// TODO: add seccomp_profile after P0223` and the field lands in a follow-up.

Pattern from [`rio-controller/src/crds/workerpool.rs:31-44,224-235`](../../rio-controller/src/crds/workerpool.rs) — same `#[derive(CustomResource, KubeSchema)]` + `#[kube(...)]` attributes.

## Entry criteria

- [P0223](plan-0223-seccomp-localhost-profile.md) merged (`SeccompProfileKind` enum exists in `crds/workerpool.rs`; `PoolTemplate` references it)

## Tasks

### T1 — `feat(controller):` WorkerPoolSet CRD types

NEW `rio-controller/src/crds/workerpoolset.rs` (~150 lines):

```rust
use kube::CustomResource;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use super::workerpool::SeccompProfileKind;

/// WorkerPoolSet manages multiple size-class WorkerPools as a unit.
/// Each class gets a child WorkerPool; the controller keeps them in sync.
#[derive(CustomResource, Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[kube(
    group = "rio.build",
    version = "v1alpha1",
    kind = "WorkerPoolSet",
    namespaced,
    status = "WorkerPoolSetStatus",
    shortname = "wps",
)]
#[serde(rename_all = "camelCase")]
pub struct WorkerPoolSetSpec {
    /// Size classes. Each becomes a child WorkerPool named `{wps}-{class.name}`.
    pub classes: Vec<SizeClassSpec>,

    /// Template merged into each child WorkerPool's spec.
    pub pool_template: PoolTemplate,

    /// Cutoff learning config (controls the rebalancer).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cutoff_learning: Option<CutoffLearningConfig>,
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct SizeClassSpec {
    pub name: String,
    pub cutoff_secs: f64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub min_replicas: Option<i32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_replicas: Option<i32>,
    /// k8s resource requests/limits for this class's workers.
    #[schemars(schema_with = "any_object")]  // passthrough: k8s ResourceRequirements
    pub resources: serde_json::Value,
}

/// Subset of WorkerPoolSpec shared across all child pools.
/// Merged with per-class fields in the child builder.
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct PoolTemplate {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub image: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub node_selector: Option<std::collections::BTreeMap<String, String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tolerations: Option<Vec<serde_json::Value>>,  // any_object passthrough
    /// Inherited from WorkerPoolSpec (P0223). Applied uniformly across classes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub seccomp_profile: Option<SeccompProfileKind>,
    // ... other shared fields from WorkerPoolSpec
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct CutoffLearningConfig {
    pub enabled: bool,
    #[serde(default = "default_min_samples")]
    pub min_samples: u64,
    #[serde(default = "default_ema_alpha")]
    pub ema_alpha: f64,
}
fn default_min_samples() -> u64 { 100 }
fn default_ema_alpha() -> f64 { 0.3 }

#[derive(Clone, Debug, Default, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct WorkerPoolSetStatus {
    pub classes: Vec<ClassStatus>,
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ClassStatus {
    pub name: String,
    pub effective_cutoff_secs: f64,
    pub queued: u64,
    pub child_pool: String,  // name of the owned WorkerPool
    pub replicas: i32,
}

/// schemars helper: passthrough schema for embedded k8s types.
/// Same pattern as workerpool.rs — avoids duplicating k8s type schemas.
fn any_object(_: &mut schemars::gen::SchemaGenerator) -> schemars::schema::Schema {
    serde_json::from_value(serde_json::json!({
        "type": "object",
        "x-kubernetes-preserve-unknown-fields": true,
    })).unwrap()
}
```

**`KubeSchema` check:** verify `kube-derive` version supports `#[derive(KubeSchema)]` or CEL validation (phase4c.md:26 mentions it). `grep KubeSchema rio-controller/src/crds/workerpool.rs` — if workerpool.rs already uses it, copy the pattern verbatim.

### T2 — `feat(controller):` mod decl + crdgen registration

MODIFY [`rio-controller/src/crds/mod.rs`](../../rio-controller/src/crds/mod.rs) — one line:

```rust
pub mod workerpoolset;
```

MODIFY [`rio-controller/src/bin/crdgen.rs`](../../rio-controller/src/bin/crdgen.rs) — add to the CRD yaml output loop:

```rust
print_crd::<rio_controller::crds::workerpoolset::WorkerPoolSet>();
```

**DO NOT commit regenerated `infra/helm/crds/*.yaml` here** — P0235 does that. This plan proves the struct compiles and `crdgen` emits valid YAML.

## Exit criteria

- `/nbr .#ci` green
- `nix-build-remote --no-nom --dev -- .#crds` produces YAML output including a `WorkerPoolSet` CRD (grep `kind: CustomResourceDefinition` + `kind: WorkerPoolSet` in result) — **do not commit the yaml**
- `PoolTemplate` has `seccomp_profile: Option<SeccompProfileKind>` (imported from workerpool.rs)

## Tracey

No markers — CRD struct definitions are type plumbing. The reconciler (P0233) gets `r[ctrl.wps.reconcile]`.

## Files

```json files
[
  {"path": "rio-controller/src/crds/workerpoolset.rs", "action": "NEW", "note": "T1: WorkerPoolSetSpec + SizeClassSpec + PoolTemplate (inherits seccomp_profile) + CutoffLearningConfig + Status types (~150 lines)"},
  {"path": "rio-controller/src/crds/mod.rs", "action": "MODIFY", "note": "T2: pub mod workerpoolset (1 line)"},
  {"path": "rio-controller/src/bin/crdgen.rs", "action": "MODIFY", "note": "T2: print_crd::<WorkerPoolSet>() registration"}
]
```

```
rio-controller/src/
├── crds/
│   ├── workerpoolset.rs   # T1 (NEW): all CRD types
│   └── mod.rs             # T2: pub mod (1 line)
└── bin/crdgen.rs          # T2: registration
```

## Dependencies

```json deps
{"deps": [223], "soft_deps": [], "note": "WPS spine hop 1. deps:[P0223(type)] — PoolTemplate inherits SeccompProfileKind. All new files + 2 one-line appends — zero conflict."}
```

**Depends on:** [P0223](plan-0223-seccomp-localhost-profile.md) — `SeccompProfileKind` enum must exist in `crds/workerpool.rs`; `PoolTemplate` imports it.
**Conflicts with:** none — `workerpoolset.rs` is NEW; `crds/mod.rs` and `crdgen.rs` are one-line appends.

**Hidden check at dispatch:** `grep KubeSchema rio-controller/src/crds/workerpool.rs` — if present, copy the pattern; if absent, `#[derive(CustomResource, JsonSchema)]` alone is sufficient for the schema.
