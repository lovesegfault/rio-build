# Plan 0226: Verification gaps — cross-tenant ListBuilds + __noChroot tracey

phase4c.md:64-65 — two verification gaps. (a) cross-tenant isolation in `ListBuilds` lacks a test: does filtering by tenant A actually exclude tenant B's builds? (b) `__noChroot` rejection is **fully implemented** (GT2: [`handler/build.rs:522-540`](../../rio-gateway/src/handler/build.rs), [`translate.rs:280-333`](../../rio-gateway/src/translate.rs)) but has no tracey marker.

**Part (b) is annotation-only.** The code exists and works (verified on disk). This plan adds: (1) spec ¶ in gateway.md, (2) `// r[impl ...]` comments on the existing code, (3) a golden test `// r[verify ...]`. Budget: ~1h if a NEW golden test is needed (per the `translate.rs:646` comment: "__noChroot rejection hard to unit-test here" — may need a fake drv_cache entry).

## Tasks

### T1 — `test(scheduler):` cross-tenant ListBuilds isolation

MODIFY [`rio-scheduler/src/admin/tests.rs`](../../rio-scheduler/src/admin/tests.rs) — new test that seeds 2 tenants + 2 builds and filters:

```rust
#[tokio::test]
async fn list_builds_filters_by_tenant() {
    let (db, _pg) = test_db().await;
    let tenant_a = seed_tenant(&db, "tenant-a").await;
    let tenant_b = seed_tenant(&db, "tenant-b").await;
    let build_a = seed_build(&db, tenant_a, "drv-a").await;
    let build_b = seed_build(&db, tenant_b, "drv-b").await;

    // Filter by tenant A → only build_a appears
    let resp = list_builds(&db, ListBuildsRequest {
        tenant_id: Some(tenant_a),
        ..Default::default()
    }).await.unwrap();
    assert_eq!(resp.builds.len(), 1);
    assert_eq!(resp.builds[0].build_id, build_a);

    // Filter by tenant B → only build_b
    let resp = list_builds(&db, ListBuildsRequest {
        tenant_id: Some(tenant_b),
        ..Default::default()
    }).await.unwrap();
    assert_eq!(resp.builds.len(), 1);
    assert_eq!(resp.builds[0].build_id, build_b);

    // No filter → both
    let resp = list_builds(&db, ListBuildsRequest::default()).await.unwrap();
    assert_eq!(resp.builds.len(), 2);
}
```

### T2 — `docs(gateway):` r[gw.reject.nochroot] spec ¶

MODIFY [`docs/src/components/gateway.md`](../../docs/src/components/gateway.md) — add the spec paragraph in the `## DAG Reconstruction` section (near `:454`):

```markdown
r[gw.reject.nochroot]
The gateway MUST reject any derivation (at SubmitBuild time) whose env contains `__noChroot = "1"`. This is a sandbox-escape request that rio-build does not honor. Rejection happens at two points: (1) `validate_dag` checks every node's drv_cache entry before the gRPC SubmitBuild call; (2) `wopBuildDerivation` handler checks the inline BasicDerivation's env directly (covering the case where the full drv is not in the cache). Both paths send `STDERR_ERROR` with a "sandbox escape — not permitted" message. The scheduler does not see the `__noChroot` env (DerivationNode doesn't carry it), so this check is gateway-only.
```

### T3 — `docs(gateway):` r[impl] comments on existing code

MODIFY [`rio-gateway/src/translate.rs`](../../rio-gateway/src/translate.rs) — add `// r[impl gw.reject.nochroot]` above the `validate_dag` __noChroot loop (near `:280-333`). Comment-only change.

MODIFY [`rio-gateway/src/handler/build.rs`](../../rio-gateway/src/handler/build.rs) — add `// r[impl gw.reject.nochroot]` above the inline-BasicDerivation check at `:522-540`. Comment-only change.

### T4 — `test(gateway):` golden test for __noChroot rejection

Check if an existing golden test covers `__noChroot`. Grep:
```bash
grep -rn noChroot rio-gateway/tests/
```

**If one exists:** add `// r[verify gw.reject.nochroot]` comment above it. Done.

**If none exists:** NEW golden test in `rio-gateway/tests/golden/` (or `rio-gateway/src/translate.rs` test module):

```rust
// r[verify gw.reject.nochroot]
#[test]
fn validate_dag_rejects_nochroot() {
    let drv = Derivation {
        env: HashMap::from([("__noChroot".into(), "1".into())]),
        ..fake_drv()
    };
    let store_path = fake_store_path("bad.drv");
    let drv_cache = HashMap::from([(store_path.clone(), drv)]);
    let nodes = vec![DerivationNode { drv_path: store_path.to_string(), ..Default::default() }];

    let err = validate_dag(&nodes, &drv_cache).unwrap_err();
    assert!(err.contains("__noChroot"));
    assert!(err.contains("not permitted") || err.contains("sandbox"));
}
```

The `translate.rs:646` comment warns this is "hard to unit-test here" — but `validate_dag` is a pure function taking `&[DerivationNode]` + `&HashMap`. The hard part was the `handler/build.rs` inline-BasicDerivation path (needs wire bytes). Unit-test `validate_dag`; the inline path gets a separate test if straightforward, else defer with a note.

## Exit criteria

- `/nbr .#ci` green
- `list_builds_filters_by_tenant` passes (tenant A filter returns exactly build_a)
- `tracey query rule gw.reject.nochroot` shows spec + impl (2 sites) + verify

## Tracey

Adds new marker to component specs:
- `r[gw.reject.nochroot]` → [`docs/src/components/gateway.md`](../../docs/src/components/gateway.md) (see ## Spec additions below)

## Spec additions

**`r[gw.reject.nochroot]`** — placed in `docs/src/components/gateway.md` in the `## DAG Reconstruction` section, standalone paragraph, blank line before, col 0:

```
r[gw.reject.nochroot]
The gateway MUST reject any derivation (at SubmitBuild time) whose env contains `__noChroot = "1"`. This is a sandbox-escape request that rio-build does not honor. Rejection happens at two points: (1) `validate_dag` checks every node's drv_cache entry before the gRPC SubmitBuild call; (2) `wopBuildDerivation` handler checks the inline BasicDerivation's env directly (covering the case where the full drv is not in the cache). Both paths send `STDERR_ERROR` with a "sandbox escape — not permitted" message. The scheduler does not see the `__noChroot` env (DerivationNode doesn't carry it), so this check is gateway-only.
```

## Files

```json files
[
  {"path": "rio-scheduler/src/admin/tests.rs", "action": "MODIFY", "note": "T1: list_builds_filters_by_tenant (2 tenants, 2 builds, filter check)"},
  {"path": "docs/src/components/gateway.md", "action": "MODIFY", "note": "T2: r[gw.reject.nochroot] spec ¶ in DAG Reconstruction section"},
  {"path": "rio-gateway/src/translate.rs", "action": "MODIFY", "note": "T3+T4: r[impl] comment at :280 + r[verify] validate_dag_rejects_nochroot test"},
  {"path": "rio-gateway/src/handler/build.rs", "action": "MODIFY", "note": "T3: r[impl] comment ONLY at :522 (no code change)"}
]
```

```
rio-scheduler/src/admin/tests.rs   # T1: cross-tenant test
docs/src/components/gateway.md     # T2: spec ¶
rio-gateway/src/
├── translate.rs                   # T3: r[impl] comment + T4 test
└── handler/build.rs               # T3: r[impl] comment ONLY
```

## Dependencies

```json deps
{"deps": [], "soft_deps": [], "note": "Wave-1 frontier. handler/build.rs count=19 but comment-only touch — safe parallel with any plan."}
```

**Depends on:** none. Code already exists (GT2); this is annotation + test coverage.
**Conflicts with:** `handler/build.rs` count=19 but this is a **comment-only** change — no semantic conflict possible with any parallel plan. `translate.rs` not in hot matrix.
