# Plan 0238: BuildStatus fine-grained conditions (fields DEFERRED to Phase 5)

phase4c.md:60 + **A3 accepted** — wire three conditions (Scheduled / InputsResolved / Building) into the `Build` CR's status via the `drain_stream` event handler. The `criticalPathRemaining` + `workers` fields are **DEFERRED to Phase 5** per A3 — phase4c.md:60 explicitly permits this ("include if straightforward, else defer the fields"). If BuildEvent already carries worker IDs, the fields are trivial and can land here too — check at dispatch.

**SSA condition patch pattern from [`scaling.rs:436,506`](../../rio-controller/src/scaling.rs)** — `scaling_condition(status, reason, message)`. Key subtlety: `lastTransitionTime` only updates on **status change** (True→False or False→True), NOT on every event. Setting it on every event makes `kubectl get build -w` noise.

**R5 — SSA apiVersion+kind.** Same 3a-bug mitigation as P0234. SSA patch inside the event loop MUST include `apiVersion` + `kind`. Test asserts `.metadata.managedFields` entry, not just status value.

## Tasks

### T1 — `feat(controller):` Build conditions in drain_stream

MODIFY [`rio-controller/src/reconcilers/build.rs`](../../rio-controller/src/reconcilers/build.rs) — in `drain_stream` at `:357` and `:426` (the BuildEvent match arms):

```rust
// Map BuildEvent → condition transition.
// lastTransitionTime only on STATUS CHANGE — track prev state.
let new_conditions = match event {
    BuildEvent::Scheduled { .. } => vec![
        build_condition("Scheduled", "True", "Scheduled", "build assigned to scheduler"),
    ],
    BuildEvent::InputsResolved { .. } => vec![
        build_condition("InputsResolved", "True", "InputsAvailable", "all input paths substituted"),
    ],
    BuildEvent::Building { .. } => vec![
        build_condition("Building", "True", "WorkerAssigned", "derivation dispatched to worker"),
    ],
    _ => vec![],
};

if !new_conditions.is_empty() {
    // SSA status patch — apiVersion+kind MANDATORY (3a bug)
    let ar = Build::api_resource();
    let patch = serde_json::json!({
        "apiVersion": ar.api_version,
        "kind": ar.kind,
        "status": {
            "conditions": new_conditions,
            // TODO(phase5): criticalPathRemaining, workers — A3 deferred.
            // If BuildEvent already carries worker IDs, revisit here.
        },
    });
    build_api.patch_status(
        &build_name,
        &PatchParams::apply("rio-controller-build-conditions").force(),
        &Patch::Apply(&patch),
    ).await?;
}
```

`build_condition` helper mirroring `scaling_condition` at [`scaling.rs:506`](../../rio-controller/src/scaling.rs):

```rust
fn build_condition(type_: &str, status: &str, reason: &str, message: &str) -> serde_json::Value {
    serde_json::json!({
        "type": type_,
        "status": status,
        "reason": reason,
        "message": message,
        "lastTransitionTime": chrono::Utc::now().to_rfc3339(),
    })
}
```

**lastTransitionTime subtlety:** the above sets it every time. If that's too noisy: track previous condition state in-memory (HashMap<BuildUid, HashMap<CondType, Status>>) and only include `lastTransitionTime` when the status value changed. Decide at implement based on event frequency.

### T2 — `feat(controller):` condition types on Build CRD (if needed)

MODIFY [`rio-controller/src/crds/build.rs`](../../rio-controller/src/crds/build.rs) — if `BuildStatus` doesn't already have a `conditions: Vec<Condition>` field with the standard k8s Condition shape, add it. Check first:

```bash
grep -A5 'struct BuildStatus' rio-controller/src/crds/build.rs
```

If conditions are already there (many CRDs ship with them), no change needed.

### T3 — `test(controller):` condition transition test

```rust
#[tokio::test]
async fn build_conditions_transition_on_events() {
    let (build_api, _mock) = mock_build_api();
    let events = vec![
        BuildEvent::Scheduled { build_id: 1 },
        BuildEvent::InputsResolved { build_id: 1 },
        BuildEvent::Building { build_id: 1, drv_path: "...".into() },
    ];

    for ev in events {
        handle_event(&build_api, ev).await.unwrap();
    }

    let build = build_api.get("test-build").await.unwrap();
    let conds = build.status.unwrap().conditions;

    // All three conditions present with status=True
    assert!(conds.iter().any(|c| c.type_ == "Scheduled" && c.status == "True"));
    assert!(conds.iter().any(|c| c.type_ == "InputsResolved" && c.status == "True"));
    assert!(conds.iter().any(|c| c.type_ == "Building" && c.status == "True"));

    // managedFields has our field manager (proves SSA worked)
    let mf = build.metadata.managed_fields.unwrap();
    assert!(mf.iter().any(|m| m.manager.as_deref() == Some("rio-controller-build-conditions")));
}
```

### T4 — `docs(controller):` close deferrals + TODO(phase5)

MODIFY [`docs/src/components/controller.md`](../../docs/src/components/controller.md) — update `:30` and `:43` deferral text from "not implemented" to "conditions implemented in Phase 4c; `criticalPathRemaining` + `workers` fields deferred to Phase 5 (see A3)."

## Exit criteria

- `/nbr .#ci` green
- `build_conditions_transition_on_events` passes (all 3 conditions present, managedFields has `rio-controller-build-conditions`)
- `controller.md:30,:43` updated to "conditions implemented, fields deferred Phase 5"
- `TODO(phase5)` comment in code for the deferred fields

## Tracey

No markers — conditions are observability/UX, not normative spec behavior. The Build CRD lifecycle markers (`r[ctrl.build.*]`) already exist at [`controller.md:272-278`](../../docs/src/components/controller.md).

## Files

```json files
[
  {"path": "rio-controller/src/reconcilers/build.rs", "action": "MODIFY", "note": "T1+T3: condition SSA patch in drain_stream at :357,:426 + build_condition helper + unit test. apiVersion+kind MANDATORY. TODO(phase5) for deferred fields."},
  {"path": "rio-controller/src/crds/build.rs", "action": "MODIFY", "note": "T2: conditions field on BuildStatus if not already present (check first)"},
  {"path": "docs/src/components/controller.md", "action": "MODIFY", "note": "T4: update :30,:43 to 'conditions implemented, fields deferred Phase 5'"}
]
```

```
rio-controller/src/
├── reconcilers/build.rs   # T1+T3: conditions + test
└── crds/build.rs          # T2: conditions field (if needed)
docs/src/components/controller.md  # T4: close deferrals
```

## Dependencies

```json deps
{"deps": [], "soft_deps": [], "note": "Wave-1 frontier. No 4b plan touches reconcilers/build.rs (verified). Single 4c touch — no serialization."}
```

**Depends on:** none. No 4b plan found touching `reconcilers/build.rs`.
**Conflicts with:** none — single 4c touch on `reconcilers/build.rs`. `crds/build.rs` is only touched here (P0232 adds `workerpoolset.rs`, not `build.rs`).
