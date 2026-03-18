# Plan 0234: WPS status refresh + per-class autoscaler — Y-JOIN

phase4c.md:31,33,91 — the **Y-JOIN** where the SITA-E spine (via [P0231](plan-0231-get-sizeclass-status-rpc-hub.md) `GetSizeClassStatus`) converges with the WPS spine (via [P0233](plan-0233-wps-child-builder-reconciler.md) reconciler). Two additions: (a) status refresh block in `workerpoolset/mod.rs` — call `GetSizeClassStatus`, write `effective_cutoff` + `queued` per class to WPS status via SSA patch; (b) per-class autoscaler hook at [`scaling.rs:195`](../../rio-controller/src/scaling.rs) — iterate classes, `compute_desired(queued, target, min, max)` per child, SSA-patch child replicas.

**SSA apiVersion+kind — THE 3a BUG.** Per [lang-gotchas memory](../../.claude/memory/lang-gotchas.md): SSA patches MUST carry `apiVersion` + `kind` in the body. Without them, the apiserver silently no-ops or returns a cryptic 422. The correct pattern is at [`workerpool/mod.rs:234-247`](../../rio-controller/src/reconcilers/workerpool/mod.rs):

```rust
let ar = WorkerPool::api_resource();
let status_patch = serde_json::json!({
    "apiVersion": ar.api_version,  // MANDATORY
    "kind": ar.kind,               // MANDATORY
    "status": { ... }
});
```

**Test asserts `.metadata.managedFields` entry, NOT just the value.** Per the same memory: checking that `status.replicas == 3` doesn't prove SSA worked — a non-SSA patch also sets the value. Check that `.metadata.managedFields` contains an entry with your field manager name.

## Entry criteria

- [P0231](plan-0231-get-sizeclass-status-rpc-hub.md) merged (`GetSizeClassStatus` RPC + generated proto types exist)
- [P0233](plan-0233-wps-child-builder-reconciler.md) merged (`workerpoolset/mod.rs` reconcile loop exists — this plan adds the status block)

## Tasks

### T1 — `feat(controller):` WPS status refresh block

MODIFY [`rio-controller/src/reconcilers/workerpoolset/mod.rs`](../../rio-controller/src/reconcilers/workerpoolset/mod.rs) — add to `apply()` after the child SSA-apply loop:

```rust
// r[impl ctrl.wps.cutoff-status]
// Status refresh: call GetSizeClassStatus, write per-class effective
// cutoffs + queue depths. SSA patch — MUST include apiVersion+kind.
let admin = ctx.admin_client();  // check: does controller have this? §8 hidden-dep
let resp = admin.get_size_class_status(GetSizeClassStatusRequest {}).await?;

let class_statuses: Vec<ClassStatus> = wps.spec.classes.iter().map(|class| {
    let rpc_class = resp.classes.iter().find(|c| c.name == class.name);
    ClassStatus {
        name: class.name.clone(),
        effective_cutoff_secs: rpc_class.map(|c| c.effective_cutoff_secs).unwrap_or(class.cutoff_secs),
        queued: rpc_class.map(|c| c.queued).unwrap_or(0),
        child_pool: format!("{}-{}", wps.name_any(), class.name),
        replicas: 0,  // filled below from child WorkerPool status
    }
}).collect();

let wps_api: Api<WorkerPoolSet> = Api::namespaced(ctx.client.clone(), &ns);
let ar = WorkerPoolSet::api_resource();
let status_patch = serde_json::json!({
    "apiVersion": ar.api_version,   // MANDATORY — 3a bug
    "kind": ar.kind,                // MANDATORY
    "status": {
        "classes": class_statuses,
    },
});
wps_api.patch_status(
    &wps.name_any(),
    &PatchParams::apply("rio-controller-wps-status").force(),
    &Patch::Apply(&status_patch),
).await?;
```

### T2 — `feat(controller):` per-class autoscaler hook

MODIFY [`rio-controller/src/scaling.rs`](../../rio-controller/src/scaling.rs) — at `:195` (the scaling loop hook point), add WPS-aware per-class scaling:

```rust
// r[impl ctrl.wps.autoscale]
// Per-class autoscaler: call GetSizeClassStatus, compute desired
// replicas per child pool based on per-class queue depth.
async fn scale_workerpoolset_children(
    wps: &WorkerPoolSet,
    admin: &AdminClient,
    wp_api: &Api<WorkerPool>,
) -> Result<(), Error> {
    let resp = admin.get_size_class_status(GetSizeClassStatusRequest {}).await?;

    for class in &wps.spec.classes {
        let rpc_class = resp.classes.iter().find(|c| c.name == class.name);
        let queued = rpc_class.map(|c| c.queued).unwrap_or(0);

        let desired = compute_desired(
            queued,
            class.min_replicas.unwrap_or(1),
            class.max_replicas.unwrap_or(10),
        );

        let child_name = format!("{}-{}", wps.name_any(), class.name);

        // Skip-deleting guard (pattern from :227-230): don't scale a
        // pool that's being deleted.
        if let Ok(child) = wp_api.get(&child_name).await {
            if child.metadata.deletion_timestamp.is_some() { continue; }
        }

        // SSA replica patch with DISTINCT field manager so it doesn't
        // fight the reconciler's apply. Pattern from :338.
        let ar = WorkerPool::api_resource();
        let replica_patch = serde_json::json!({
            "apiVersion": ar.api_version,
            "kind": ar.kind,
            "spec": { "replicas": desired },
        });
        wp_api.patch(
            &child_name,
            &PatchParams::apply("rio-controller-wps-autoscaler").force(),
            &Patch::Apply(&replica_patch),
        ).await?;
    }
    Ok(())
}
```

### T3 — `test(controller):` SSA managedFields assertion

Unit test that mocks `GetSizeClassStatus` → verifies replica patch:

```rust
// r[verify ctrl.wps.autoscale]
// r[verify ctrl.wps.cutoff-status]
#[tokio::test]
async fn wps_autoscaler_writes_via_ssa_field_manager() {
    let (wps, wp_api) = test_wps_with_mock_api(&["small"]);
    let admin = mock_admin_returning(GetSizeClassStatusResponse {
        classes: vec![SizeClassStatus { name: "small".into(), queued: 20, ..Default::default() }],
    });

    scale_workerpoolset_children(&wps, &admin, &wp_api).await.unwrap();

    // Assert managedFields — NOT just the replica value.
    // A non-SSA patch also sets replicas; only SSA sets managedFields.
    let child = wp_api.get("test-wps-small").await.unwrap();
    let mf = child.metadata.managed_fields.unwrap();
    assert!(mf.iter().any(|m| m.manager.as_deref() == Some("rio-controller-wps-autoscaler")),
            "expected rio-controller-wps-autoscaler in managedFields, got {:?}", mf);
}
```

### T4 — `docs(controller):` spec ¶s + close challenges.md deferral

MODIFY [`docs/src/components/controller.md`](../../docs/src/components/controller.md) — add both spec paragraphs after `r[ctrl.wps.reconcile]` (from P0233):

```markdown
r[ctrl.wps.cutoff-status]
The WPS reconciler writes per-class `effective_cutoff_secs` + `queued` to WPS status via SSA patch (field manager `rio-controller-wps-status`). Values come from the `GetSizeClassStatus` admin RPC. SSA patch body MUST include `apiVersion` + `kind`.

r[ctrl.wps.autoscale]
Per-class autoscaling: for each WPS child WorkerPool, compute `desired = clamp(queued / target_queue_per_replica, min_replicas, max_replicas)` and SSA-patch `spec.replicas` with field manager `rio-controller-wps-autoscaler` (distinct from the reconciler's field manager — SSA merges field ownership). Skip children with non-nil `deletionTimestamp`.
```

MODIFY [`docs/src/challenges.md`](../../docs/src/challenges.md) — close the `:138` WPS deferral.

## Exit criteria

- `/nbr .#ci` green
- `tracey query rule ctrl.wps.autoscale` shows spec + impl + verify
- `tracey query rule ctrl.wps.cutoff-status` shows spec + impl + verify
- `wps_autoscaler_writes_via_ssa_field_manager` passes — asserts `managedFields` contains `rio-controller-wps-autoscaler`, NOT just replica value

## Tracey

Adds new markers to component specs:
- `r[ctrl.wps.cutoff-status]` → [`docs/src/components/controller.md`](../../docs/src/components/controller.md) (see ## Spec additions below)
- `r[ctrl.wps.autoscale]` → [`docs/src/components/controller.md`](../../docs/src/components/controller.md) (see ## Spec additions below)

**`r[verify ctrl.wps.autoscale]` also lands in [P0239](plan-0239-vm-pdb-section-h-wps.md)** (VM lifecycle test) — the unit test here covers the SSA mechanics; the VM test covers end-to-end.

## Spec additions

**`r[ctrl.wps.cutoff-status]`** — placed in `docs/src/components/controller.md` after `r[ctrl.wps.reconcile]`, standalone paragraph:

```
r[ctrl.wps.cutoff-status]
The WPS reconciler writes per-class `effective_cutoff_secs` + `queued` to WPS status via SSA patch (field manager `rio-controller-wps-status`). Values come from the `GetSizeClassStatus` admin RPC. SSA patch body MUST include `apiVersion` + `kind`.
```

**`r[ctrl.wps.autoscale]`** — placed after `r[ctrl.wps.cutoff-status]`, standalone paragraph:

```
r[ctrl.wps.autoscale]
Per-class autoscaling: for each WPS child WorkerPool, compute `desired = clamp(queued / target_queue_per_replica, min_replicas, max_replicas)` and SSA-patch `spec.replicas` with field manager `rio-controller-wps-autoscaler` (distinct from the reconciler's field manager — SSA merges field ownership). Skip children with non-nil `deletionTimestamp`.
```

## Files

```json files
[
  {"path": "rio-controller/src/reconcilers/workerpoolset/mod.rs", "action": "MODIFY", "note": "T1: status refresh block in apply() — GetSizeClassStatus + SSA patch WITH apiVersion+kind; r[impl ctrl.wps.cutoff-status]"},
  {"path": "rio-controller/src/scaling.rs", "action": "MODIFY", "note": "T2+T3: scale_workerpoolset_children at :195 + managedFields test; r[impl]+r[verify ctrl.wps.autoscale]"},
  {"path": "docs/src/components/controller.md", "action": "MODIFY", "note": "T4: r[ctrl.wps.cutoff-status] + r[ctrl.wps.autoscale] spec ¶s"},
  {"path": "docs/src/challenges.md", "action": "MODIFY", "note": "T4: close :138 WPS deferral"}
]
```

```
rio-controller/src/
├── reconcilers/workerpoolset/mod.rs  # T1: status block (serial after P0233)
└── scaling.rs                        # T2+T3: per-class hook at :195 (count=8)
docs/src/
├── components/controller.md          # T4: 2 spec ¶s
└── challenges.md                     # T4: close :138
```

## Dependencies

```json deps
{"deps": [231, 233], "soft_deps": [], "note": "Y-JOIN of both spines. deps:[P0231(proto hub), P0233(reconciler module)]. scaling.rs count=8, single 4c touch. workerpoolset/mod.rs serial via P0233 dep."}
```

**Depends on:** [P0231](plan-0231-get-sizeclass-status-rpc-hub.md) — `GetSizeClassStatus` RPC + generated proto types. [P0233](plan-0233-wps-child-builder-reconciler.md) — `workerpoolset/mod.rs` exists with the `apply()` skeleton.
**Conflicts with:** `scaling.rs` count=8 — single 4c touch. `workerpoolset/mod.rs` serial via P0233 dep.

**Hidden check at dispatch:** `grep -rn 'connect_admin\|AdminServiceClient' rio-controller/src/` — verify the controller has an admin gRPC client constructed. If absent, add `ctx.admin_client` setup in `main.rs` (small addition, but coordinate with P0235 which also touches main.rs).
