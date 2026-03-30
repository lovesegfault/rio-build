# Plan 502: ADR-020 phase 2 — BuilderPool.spec.sizing enum + CEL validation

[ADR-020](../../docs/src/decisions/020-per-derivation-capacity-manifest.md) introduces a `sizing: {Static, Manifest}` mode gate on `BuilderPoolSpec`. `Static` preserves today's fixed-spec path (STS with operator-chosen `resources`). `Manifest` enables per-derivation sizing via the `GetCapacityManifest` poll loop (P0501 → P0503).

The CEL constraint: `Manifest` requires `maxConcurrentBuilds == 1`. ADR-020 § Decision ¶4 — pods are long-lived and take any derivation that fits, but placement is by `memory_total_bytes >= est_memory_bytes`, which is a per-derivation fit check. A pod running 2 builds concurrently could accept a second build that fits individually but not alongside the first. One-derivation-per-pod is the manifest invariant.

This directly follows the ephemeral CEL pattern at [`rio-crds/src/builderpool.rs:45-62`](../../rio-crds/src/builderpool.rs) — same `#[x_kube(validation = Rule::new(...))]` attribute, same explanatory comment style.

## Tasks

### T1 — `feat(crds):` add Sizing enum + sizing field to BuilderPoolSpec

MODIFY [`rio-crds/src/builderpool.rs`](../../rio-crds/src/builderpool.rs). Add enum before `BuilderPoolSpec`:

```rust
/// Pod sizing mode. ADR-020.
///
/// `Static`: operator sets `spec.resources`, controller creates STS
/// with those resources. Preserves ADR-015 behavior. Default.
///
/// `Manifest`: controller polls `GetCapacityManifest` and spawns Jobs
/// with per-derivation resources. `spec.resources` becomes the cold-start
/// FLOOR (used when the manifest omits a derivation — no build_history).
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq, JsonSchema)]
pub enum Sizing {
    #[default]
    Static,
    Manifest,
}
```

Add field to `BuilderPoolSpec`:

```rust
#[serde(default)]
pub sizing: Sizing,
```

`#[serde(default)]` + `#[default] Static` means existing BuilderPool YAMLs without a `sizing:` line parse as `Static` — no migration needed.

### T2 — `feat(crds):` CEL validation — Manifest requires maxConcurrentBuilds==1

Add `#[x_kube(validation)]` attribute on `BuilderPoolSpec` alongside the ephemeral CEL (~:56):

```rust
// CEL: sizing=Manifest requires maxConcurrentBuilds==1. Manifest mode's
// placement filter is per-derivation (memory_total_bytes >= est_memory);
// a pod running 2 concurrent builds could accept a second that fits
// individually but not alongside the first. One-derivation-per-pod is
// the manifest invariant. ADR-020 § Decision ¶4.
// Also: Manifest implies ephemeral-like lifecycle (Jobs, not STS) —
// but doesn't REQUIRE ephemeral:true (Manifest pods are long-lived,
// not one-shot). The overlap is maxConcurrentBuilds==1.
// r[impl ctrl.pool.manifest-single-build]
#[x_kube(
    validation = Rule::new(
        "self.sizing != 'Manifest' || self.maxConcurrentBuilds == 1"
    ).message(
        "sizing:Manifest requires maxConcurrentBuilds==1 — per-derivation resource fit breaks with concurrent builds on one pod; see ADR-020 § Decision"
    )
)]
```

CEL string comparison: `self.sizing != 'Manifest'` (kube CRD serializes enum as string). Pattern matches the ephemeral rule's `!self.ephemeral || (...)` structure.

### T3 — `chore(crds):` regen CRD YAML + helm chart

Run `cargo xtask regen crds` (or whatever the regen command is — grep xtask for the crd regen subcommand). Updates `infra/helm/rio-build/crds/builderpool.yaml` with the new field + validation.

### T4 — `test(crds):` CEL validation enforces manifest constraint

NEW case in `rio-crds/src/builderpool.rs` `#[cfg(test)]` alongside existing CEL tests. Apply a `BuilderPool` with `sizing: Manifest, maxConcurrentBuilds: 4` → expect validation reject. `sizing: Manifest, maxConcurrentBuilds: 1` → accept. `sizing: Static, maxConcurrentBuilds: 4` → accept (Static mode unconstrained).

The existing ephemeral CEL tests show the test pattern — kube client dry-run or the in-proc CEL evaluator if that's what's used.

## Exit criteria

- `Sizing` enum with `#[default] Static` — existing YAMLs parse unchanged
- CEL rejects `Manifest + maxConcurrentBuilds>1` (test)
- CEL accepts `Static + maxConcurrentBuilds>1` (test)
- Generated CRD YAML contains the new enum schema + x-kubernetes-validations rule
- Helm chart CRD file updated

## Tracey

- `r[ctrl.pool.manifest-single-build]` — NEW marker. T2 CEL attribute is `r[impl]`; T4 is `r[verify]`.

## Spec additions

Add to `docs/src/components/controller.md` after `r[ctrl.pool.ephemeral-single-build]` (~:127):

```
r[ctrl.pool.manifest-single-build]

`BuilderPool.spec.sizing=Manifest` requires `maxConcurrentBuilds == 1`. Manifest mode places derivations by resource fit (`worker.memory_total_bytes >= drv.est_memory_bytes`), which is a per-derivation check. A pod accepting two concurrent builds could accept a second that fits individually but not alongside the first. CEL-enforced at CRD admission.
```

## Files

```json files
["rio-crds/src/builderpool.rs", "infra/helm/rio-build/crds/builderpool.yaml", "docs/src/components/controller.md"]
```

## Dependencies

```json deps
[]
```

No deps. Foundational — P0503 (reconciler) reads the `sizing` field to gate behavior.

## Risks

- CRD schema change means `kubectl apply -f crds/` needed on deployed clusters. No migration (additive field with default) but operators need to re-apply CRDs before deploying a controller that reads `spec.sizing`. Note this in the plan's deploy section.
- If CEL string-enum comparison syntax is wrong (`'Manifest'` vs something else), the rule silently never fires. T4 MUST include a positive reject case, not just accepts.
