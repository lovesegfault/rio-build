# Plan 0372: migrate_finalizer resourceVersion optimistic-lock — foreign-finalizer stomp risk

[`rio-controller/src/reconcilers/workerpool/mod.rs:153-189`](../../rio-controller/src/reconcilers/workerpool/mod.rs) `migrate_finalizer` uses `Patch::Merge` on `metadata.finalizers` with the **full array**. The doc-comment at `:138-141` claims "Atomic: JSON merge on `metadata.finalizers` replaces the entire array. There's no window where NEITHER finalizer blocks deletion." — true for the OLD→NEW swap, but **not** for concurrent additions: if a foreign controller adds a finalizer between the `:162` `obj.finalizers()` read and the `:179` `api.patch()` write, the merge-patch silently stomps it (merge-patch of an array = replace, not merge-by-element). The foreign controller's finalizer vanishes; if its cleanup is non-idempotent, resources leak.

`kube-rs` [`kube::runtime::finalizer::finalizer`](https://docs.rs/kube-runtime/latest/kube_runtime/finalizer/fn.finalizer.html) uses JSON Patch `add` / `remove` operations with `resourceVersion` for exactly this reason (see [`finalizer.rs:72`](https://github.com/kube-rs/kube/blob/main/kube-runtime/src/finalizer.rs) — `AtomicU32` + `PatchParams` defaults include optimistic locking via `resourceVersion`). The fix: either (a) carry `resourceVersion` in the merge-patch so a concurrent write gets 409 Conflict and the reconciler retries, or (b) switch to JSON Patch `test` + `replace` on the specific index (preserving array structure but verifying it hasn't changed).

discovered_from=233-review (validator-confirmed stomp window). Severity: correctness — silent data loss under race.

## Entry criteria

- [P0233](plan-0233-wps-child-builder-reconciler.md) merged (migrate_finalizer exists at mod.rs:153)

## Tasks

### T1 — `fix(controller):` migrate_finalizer — add resourceVersion to Patch::Merge

MODIFY [`rio-controller/src/reconcilers/workerpool/mod.rs:179-185`](../../rio-controller/src/reconcilers/workerpool/mod.rs). The merge-patch currently sends only `{"metadata": {"finalizers": patched}}`. Add `resourceVersion`:

```rust
let rv = obj
    .meta()
    .resource_version
    .clone()
    .ok_or_else(|| Error::InvalidSpec("migrate_finalizer: object has no resourceVersion".into()))?;
api.patch(
    &name,
    &PatchParams::default(),
    &Patch::Merge(serde_json::json!({
        "metadata": {
            "resourceVersion": rv,
            "finalizers": patched
        }
    })),
)
.await
.map_err(|e| match e {
    // 409 Conflict = someone else patched between our read and write.
    // The reconciler's top-level error path requeues via
    // error_policy() → Action::requeue(5s). Next reconcile reads the
    // fresh finalizers list (including whatever the foreign controller
    // added) and migrates correctly.
    kube::Error::Api(ae) if ae.code == 409 => {
        tracing::info!(object = %name, "migrate_finalizer: resourceVersion conflict, requeuing");
        Error::Conflict(format!("finalizer migration conflicted on {name}: {ae}"))
    }
    e => e.into(),
})?;
```

Add `Error::Conflict(String)` variant to [`rio-controller/src/error.rs`](../../rio-controller/src/error.rs) if not present. The `error_policy` at [`mod.rs`](../../rio-controller/src/reconcilers/workerpool/mod.rs) (grep `error_policy` or `requeue`) should already handle errors by requeuing — verify at dispatch, add `Conflict →` arm if it discriminates.

### T2 — `fix(controller):` doc-comment :138-146 — correct atomicity claim

MODIFY the doc-comment at [`mod.rs:138-152`](../../rio-controller/src/reconcilers/workerpool/mod.rs). The "Atomic" claim at `:138` is overclaimed. Replace with:

```rust
/// Lost-update safety: the merge-patch carries `resourceVersion` so a
/// concurrent finalizer add (foreign controller, between our read at
/// :162 and write at :179) gets 409 Conflict instead of silently
/// stomped. On 409, the reconciler requeues and retries with the
/// fresh list. Without resourceVersion, merge-patch on an array =
/// full replace — foreign finalizers added in the window vanish.
///
/// OLD→NEW atomicity (original concern) holds regardless: the swap is
/// one apiserver write, no window where NEITHER finalizer blocks.
```

### T3 — `test(controller):` migrate_finalizer preserves concurrent foreign-finalizer add

NEW test in [`rio-controller/src/reconcilers/workerpool/tests.rs`](../../rio-controller/src/reconcilers/workerpool/tests.rs) (or adjacent to existing `migrate_finalizer` tests — grep `migrate_finalizer` in tests.rs):

```rust
/// migrate_finalizer with stale resourceVersion gets 409, not stomp.
///
/// Scenario: we read finalizers=[OLD], meanwhile a foreign controller
/// adds "example.com/cleanup". Our merge-patch with the stale
/// resourceVersion MUST be rejected (409 Conflict), NOT succeed and
/// silently drop example.com/cleanup.
///
/// Before P0372: the merge-patch with [NEW] (no resourceVersion)
/// would succeed and set finalizers=[NEW], stomping the foreign
/// controller's example.com/cleanup that arrived in the window.
// r[verify ctrl.wps.reconcile]
#[tokio::test]
async fn migrate_finalizer_conflicts_on_stale_resource_version() {
    let client = test_client().await;
    let api: Api<WorkerPool> = Api::namespaced(client.clone(), "default");

    // Seed: WP with OLD_FINALIZER, rv=N.
    let wp = seed_workerpool_with_finalizers(&api, &[OLD_FINALIZER]).await;
    let stale_rv = wp.meta().resource_version.clone().unwrap();

    // Concurrent write: foreign controller adds example.com/cleanup.
    // This bumps resourceVersion to N+1.
    api.patch(
        &wp.name_any(),
        &PatchParams::default(),
        &Patch::Merge(serde_json::json!({
            "metadata": {"finalizers": [OLD_FINALIZER, "example.com/cleanup"]}
        })),
    )
    .await
    .unwrap();

    // Now: migrate_finalizer called with the STALE object (rv=N).
    // Before the fix: merge-patch with [NEW] succeeds, drops both
    //   OLD and example.com/cleanup → only NEW remains. STOMP.
    // After the fix: 409 Conflict → Error::Conflict → caller requeues.
    let result = migrate_finalizer(&api, &wp, OLD_FINALIZER, FINALIZER).await;

    assert!(
        matches!(result, Err(Error::Conflict(_))),
        "migrate_finalizer should 409 on stale resourceVersion, got {result:?}"
    );

    // Verify: example.com/cleanup still present (not stomped).
    let fresh = api.get(&wp.name_any()).await.unwrap();
    assert!(
        fresh.finalizers().contains(&"example.com/cleanup".to_string()),
        "foreign finalizer stomped: {:?}",
        fresh.finalizers()
    );
}
```

**Mock-server note:** if `tests.rs` uses a mock apiserver (grep `Client::try_from` or `tower_test::mock`), the mock must track `resourceVersion` and return 409 when the request's rv doesn't match stored rv. If the mock is dumb (ignores rv), this test can't prove the fix — either upgrade the mock or gate the test on a real k3s apiserver (same pattern as other integration tests; grep `test_client` setup). Check at dispatch; skip-if-mock-dumb with a `// TODO(P0372): mock doesn't track rv` if infra isn't there.

### T4 — `test(controller):` regression — happy-path migration still works

Adjacent to T3. The existing `migrate_finalizer` happy-path test (if any — grep `migrate_finalizer` in tests.rs) proves OLD→NEW swap works. If no such test exists, add:

```rust
#[tokio::test]
async fn migrate_finalizer_happy_path() {
    let client = test_client().await;
    let api: Api<WorkerPool> = Api::namespaced(client.clone(), "default");
    let wp = seed_workerpool_with_finalizers(&api, &[OLD_FINALIZER]).await;

    let action = migrate_finalizer(&api, &wp, OLD_FINALIZER, FINALIZER)
        .await
        .expect("no-conflict path");
    assert!(matches!(action, Some(_)), "should return Some(await_change)");

    let fresh = api.get(&wp.name_any()).await.unwrap();
    assert_eq!(fresh.finalizers(), &[FINALIZER.to_string()]);
}
```

## Exit criteria

- `/nbr .#ci` green
- `grep 'resourceVersion' rio-controller/src/reconcilers/workerpool/mod.rs | grep migrate_finalizer` — no match directly but `grep -A30 'fn migrate_finalizer' rio-controller/src/reconcilers/workerpool/mod.rs | grep resourceVersion` → ≥1 hit (T1: rv carried in patch)
- `grep 'Conflict\b' rio-controller/src/error.rs` → ≥1 hit (T1: variant added) OR existing variant reused
- `grep 'Atomic:.*replaces the entire array' rio-controller/src/reconcilers/workerpool/mod.rs` → 0 hits (T2: overclaimed comment rewritten)
- `grep 'Lost-update safety\|409 Conflict' rio-controller/src/reconcilers/workerpool/mod.rs` → ≥1 hit (T2: corrected comment)
- `cargo nextest run -p rio-controller migrate_finalizer_conflicts_on_stale_resource_version` → pass (T3)
- `cargo nextest run -p rio-controller migrate_finalizer_happy_path` → pass (T4, OR existing test name passes)
- T3 mutation check: remove `"resourceVersion": rv` from the merge-patch body → T3 fails (foreign finalizer stomped) — proves the rv is load-bearing

## Tracey

References existing markers:
- `r[ctrl.wps.reconcile]` — T3/T4 verify the finalizer migration path which is part of the WPS reconciler's reconcile_inner flow (called at [`mod.rs:107`](../../rio-controller/src/reconcilers/workerpool/mod.rs)). The marker at [`controller.md:123`](../../docs/src/components/controller.md) covers reconciler behavior; finalizer migration is a sub-step.

No new markers — finalizer migration is an implementation detail of the reconciler, not a spec-level contract (the spec at `controller.md:124` describes ownerRef-based GC, not the legacy-name rewrite).

## Files

```json files
[
  {"path": "rio-controller/src/reconcilers/workerpool/mod.rs", "action": "MODIFY", "note": "T1: +resourceVersion in merge-patch :179-185 + 409→Conflict mapping; T2: rewrite doc-comment :138-152"},
  {"path": "rio-controller/src/error.rs", "action": "MODIFY", "note": "T1: +Conflict(String) variant (or reuse existing)"},
  {"path": "rio-controller/src/reconcilers/workerpool/tests.rs", "action": "MODIFY", "note": "T3: +migrate_finalizer_conflicts_on_stale_resource_version; T4: +migrate_finalizer_happy_path (if absent)"}
]
```

```
rio-controller/src/
├── error.rs                           # T1: +Conflict variant
└── reconcilers/workerpool/
    ├── mod.rs                         # T1: rv in patch; T2: doc-comment
    └── tests.rs                       # T3+T4: conflict + happy-path tests
```

## Dependencies

```json deps
{"deps": [233], "soft_deps": [304], "note": "discovered_from=233-review (validator-confirmed stomp risk). migrate_finalizer at mod.rs:153-189 uses Patch::Merge on metadata.finalizers full-array without resourceVersion — foreign-controller finalizer added in read→patch window silently vanishes. The :138 'Atomic' doc-comment claim is true for OLD→NEW but overclaims for concurrent adds (merge-patch on array = replace). kube-runtime's finalizer() carries resourceVersion precisely for this. Soft-dep P0304-T99 (same file — POOL_LABEL hoist at mod.rs:394; T1 here edits :179-185; non-overlapping). Soft-dep P0304-T8 (error.rs — deletes SchedulerUnavailable; T1 here adds Conflict; non-overlapping). mod.rs count=11 (moderate) — T1+T2 edit localized :138-189 block, additive + rewrite; no signature change to migrate_finalizer. tests.rs count=5 — T3+T4 are additive test-fns."}
```

**Depends on:** [P0233](plan-0233-wps-child-builder-reconciler.md) — merged (`migrate_finalizer` exists).

**Conflicts with:** [P0304](plan-0304-trivial-batch-p0222-harness.md)-T99 hoists `POOL_LABEL` at `mod.rs:394` — different block from T1's `:179-185`, non-overlapping. P0304-T8 deletes `SchedulerUnavailable` from `error.rs` — T1 here adds `Conflict` — both additive-or-subtractive on different variants, rebase-clean.
