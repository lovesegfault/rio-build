# Plan 0233: WPS child builder + reconciler (no status yet)

phase4c.md:28-30,32 — the WPS reconciler loop. For each `SizeClassSpec`, build a child `WorkerPool` named `{wps}-{class.name}`, set `ownerReferences` so k8s GC collects children when the WPS is deleted, SSA-apply it. Finalizer-wrapped cleanup explicitly deletes children (belt-and-suspenders — ownerRef handles it too but explicit delete is deterministic for tests).

**Status refresh is OMITTED here** — it needs `GetSizeClassStatus` ([P0231](plan-0231-get-sizeclass-status-rpc-hub.md)), which converges at the Y-join ([P0234](plan-0234-wps-status-perclass-autoscaler-yjoin.md)). This plan writes the reconcile loop skeleton and the child builder; P0234 adds the status-patch block.

**`r[verify ctrl.wps.reconcile]` LANDS IN P0239**, not here. Between this plan's merge and P0239's merge, `tracey query untested` shows `ctrl.wps.reconcile`. **EXPECTED, TRANSIENT.** Do NOT add a dummy `r[verify]` to silence it — that defeats the tool.

Pattern from [`rio-controller/src/reconcilers/workerpool/mod.rs:84-109`](../../rio-controller/src/reconcilers/workerpool/mod.rs) (SSA apply + finalizer wrap) and [`:118-127`](../../rio-controller/src/reconcilers/workerpool/mod.rs) (`controller_owner_ref(&())`).

## Entry criteria

- [P0232](plan-0232-wps-crd-struct-crdgen.md) merged (`WorkerPoolSet` types exist in `crds/workerpoolset.rs`)

## Tasks

### T1 — `feat(controller):` child builder

NEW `rio-controller/src/reconcilers/workerpoolset/builders.rs`:

```rust
use crate::crds::workerpool::{WorkerPool, WorkerPoolSpec};
use crate::crds::workerpoolset::{WorkerPoolSet, SizeClassSpec, PoolTemplate};
use kube::ResourceExt;

/// Build one child WorkerPool for one size class.
/// Name: `{wps.name}-{class.name}`. Merges PoolTemplate (shared) + class-specific.
pub fn build_child_workerpool(
    wps: &WorkerPoolSet,
    class: &SizeClassSpec,
) -> WorkerPool {
    let template = &wps.spec.pool_template;
    let name = format!("{}-{}", wps.name_any(), class.name);

    let spec = WorkerPoolSpec {
        size_class: Some(class.name.clone()),
        replicas: class.min_replicas,  // autoscaler (P0234) adjusts within [min,max]
        resources: Some(class.resources.clone()),

        // From PoolTemplate (shared across classes):
        image: template.image.clone(),
        node_selector: template.node_selector.clone(),
        seccomp_profile: template.seccomp_profile.clone(),
        // ... other merged fields
        ..Default::default()
    };

    let mut wp = WorkerPool::new(&name, spec);

    // controller_owner_ref(&()) — the &() is because DynamicType=() for
    // static CRDs. Copy this LITERALLY from workerpool/mod.rs:127.
    // Type inference here is unforgiving.
    wp.metadata.owner_references = Some(vec![
        wps.controller_owner_ref(&()).expect("wps has uid")
    ]);
    wp.metadata.namespace = wps.metadata.namespace.clone();

    wp
}
```

### T2 — `feat(controller):` reconciler loop

NEW `rio-controller/src/reconcilers/workerpoolset/mod.rs`:

```rust
// r[impl ctrl.wps.reconcile]
//
// Reconcile: for each class in spec.classes, SSA-apply the child
// WorkerPool. k8s ownerReferences handle GC when WPS is deleted,
// but the finalizer-wrapped cleanup explicitly deletes for
// deterministic test timing.

mod builders;
use builders::build_child_workerpool;

pub const MANAGER: &str = "rio-controller-wps";
pub const FINALIZER: &str = "workerpoolset.rio.build/finalizer";

pub async fn reconcile(wps: Arc<WorkerPoolSet>, ctx: Arc<Ctx>) -> Result<Action, Error> {
    let ns = wps.namespace().expect("namespaced CRD");
    let wps_api: Api<WorkerPoolSet> = Api::namespaced(ctx.client.clone(), &ns);

    finalizer(&wps_api, FINALIZER, wps.clone(), |event| async move {
        match event {
            Event::Apply(wps) => apply(wps, ctx.clone()).await,
            Event::Cleanup(wps) => cleanup(wps, ctx.clone()).await,
        }
    }).await.map_err(|e| Error::Finalizer(Box::new(e)))
}

async fn apply(wps: Arc<WorkerPoolSet>, ctx: Arc<Ctx>) -> Result<Action, Error> {
    let ns = wps.namespace().unwrap();
    let wp_api: Api<WorkerPool> = Api::namespaced(ctx.client.clone(), &ns);

    for class in &wps.spec.classes {
        let child = build_child_workerpool(&wps, class);
        wp_api.patch(
            &child.name_any(),
            &PatchParams::apply(MANAGER).force(),
            &Patch::Apply(&child),
        ).await?;
    }

    // Status refresh: OMITTED here. P0234 adds the GetSizeClassStatus
    // call + status SSA patch. For now, reconcile succeeds without status.

    Ok(Action::requeue(Duration::from_secs(300)))
}

async fn cleanup(wps: Arc<WorkerPoolSet>, ctx: Arc<Ctx>) -> Result<Action, Error> {
    let ns = wps.namespace().unwrap();
    let wp_api: Api<WorkerPool> = Api::namespaced(ctx.client.clone(), &ns);

    // Explicit delete for deterministic timing. ownerRef GC would
    // eventually do this, but "eventually" is racy for tests.
    for class in &wps.spec.classes {
        let child_name = format!("{}-{}", wps.name_any(), class.name);
        let _ = wp_api.delete(&child_name, &DeleteParams::default()).await;
    }

    Ok(Action::await_change())
}
```

MODIFY [`rio-controller/src/reconcilers/mod.rs`](../../rio-controller/src/reconcilers/mod.rs) — one line: `pub mod workerpoolset;`.

### T3 — `test(controller):` builder unit test

Unit test in `builders.rs`:

```rust
#[test]
fn three_class_wps_yields_three_children_with_owner_ref() {
    let wps = test_wps_with_classes(&["small", "medium", "large"]);
    let children: Vec<_> = wps.spec.classes.iter()
        .map(|c| build_child_workerpool(&wps, c))
        .collect();

    assert_eq!(children.len(), 3);
    for (i, child) in children.iter().enumerate() {
        // Name = {wps}-{class}
        assert_eq!(child.name_any(), format!("test-wps-{}", wps.spec.classes[i].name));
        // ownerRef → WPS UID, controller=true
        let or = &child.metadata.owner_references.as_ref().unwrap()[0];
        assert_eq!(or.uid, wps.metadata.uid.as_deref().unwrap());
        assert_eq!(or.controller, Some(true));
        // size_class set
        assert_eq!(child.spec.size_class.as_deref(), Some(wps.spec.classes[i].name.as_str()));
    }
}
```

### T4 — `docs(controller):` close deferral blocks + spec ¶

MODIFY [`docs/src/components/controller.md`](../../docs/src/components/controller.md) — close the `:100` WPS deferral blockquote, add `r[ctrl.wps.reconcile]` spec ¶ in the `### WorkerPoolSet` section:

```markdown
r[ctrl.wps.reconcile]
The WorkerPoolSet reconciler creates one child WorkerPool per `spec.classes[i]`, named `{wps}-{class.name}`, with `ownerReferences[0].controller=true` pointing at the WPS. SSA-apply with force (field manager `rio-controller-wps`). On deletion, the finalizer-wrapped cleanup explicitly deletes children for deterministic timing; k8s ownerRef GC is the fallback.
```

MODIFY [`docs/src/components/scheduler.md`](../../docs/src/components/scheduler.md) — close the `:37,:123` WPS-related deferrals (update wording to "implemented in Phase 4c" or remove the blockquote).

## Exit criteria

- `/nbr .#ci` green
- `three_class_wps_yields_three_children_with_owner_ref` passes (3 children, correct names + ownerRef UID + controller=true)
- controller.md `:100` deferral blockquote closed; `r[ctrl.wps.reconcile]` spec ¶ present
- **NOTE: `tracey query untested` will show `ctrl.wps.reconcile` until P0239 merges — EXPECTED**

## Tracey

Adds new marker to component specs:
- `r[ctrl.wps.reconcile]` → [`docs/src/components/controller.md`](../../docs/src/components/controller.md) (see ## Spec additions below)

**`r[verify ctrl.wps.reconcile]` lands in [P0239](plan-0239-vm-pdb-section-h-wps.md)** (VM lifecycle test). Transient "untested" state between P0233 merge and P0239 merge is EXPECTED — do NOT add a dummy verify here.

## Spec additions

**`r[ctrl.wps.reconcile]`** — placed in `docs/src/components/controller.md` in the `### WorkerPoolSet` section (near current `:100`), standalone paragraph, blank line before, col 0:

```
r[ctrl.wps.reconcile]
The WorkerPoolSet reconciler creates one child WorkerPool per `spec.classes[i]`, named `{wps}-{class.name}`, with `ownerReferences[0].controller=true` pointing at the WPS. SSA-apply with force (field manager `rio-controller-wps`). On deletion, the finalizer-wrapped cleanup explicitly deletes children for deterministic timing; k8s ownerRef GC is the fallback.
```

## Files

```json files
[
  {"path": "rio-controller/src/reconcilers/workerpoolset/mod.rs", "action": "NEW", "note": "T2: reconcile loop (apply + cleanup, finalizer-wrapped); r[impl ctrl.wps.reconcile]. Status block OMITTED (P0234)."},
  {"path": "rio-controller/src/reconcilers/workerpoolset/builders.rs", "action": "NEW", "note": "T1+T3: build_child_workerpool + unit test (3 children, ownerRef check)"},
  {"path": "rio-controller/src/reconcilers/mod.rs", "action": "MODIFY", "note": "T2: pub mod workerpoolset (1 line)"},
  {"path": "docs/src/components/controller.md", "action": "MODIFY", "note": "T4: r[ctrl.wps.reconcile] spec ¶ + close :100 deferral"},
  {"path": "docs/src/components/scheduler.md", "action": "MODIFY", "note": "T4: close :37,:123 WPS deferrals"}
]
```

```
rio-controller/src/reconcilers/
├── workerpoolset/          # NEW directory
│   ├── mod.rs              # T2: r[impl] reconcile (~150 lines)
│   └── builders.rs         # T1+T3: child builder + unit test (~100 lines)
└── mod.rs                  # T2: 1-line mod decl
docs/src/components/
├── controller.md           # T4: spec ¶ + close :100 deferral
└── scheduler.md            # T4: close :37,:123 deferrals
```

## Dependencies

```json deps
{"deps": [232], "soft_deps": [], "note": "WPS spine hop 2. New directory + 1-line mod append — zero conflict. r[verify] DEFERRED to P0239 (transient untested state expected)."}
```

**Depends on:** [P0232](plan-0232-wps-crd-struct-crdgen.md) — `WorkerPoolSet` types must exist.
**Conflicts with:** none — `workerpoolset/` is a NEW directory; `reconcilers/mod.rs` is a one-line append.

**`controller_owner_ref(&())` gotcha:** the `&()` is because `DynamicType=()` for static CRDs (kube-rs type parameter). Type inference is unforgiving here — **copy the literal snippet from [`workerpool/mod.rs:127`](../../rio-controller/src/reconcilers/workerpool/mod.rs) verbatim.**
