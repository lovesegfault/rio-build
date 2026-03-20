//! Mock-apiserver reconcile-loop wiring tests.
//!
//! These test the WIRING: `apply()` calls Service PATCH → StatefulSet
//! PATCH → WorkerPool/status PATCH in that order with server-side-apply
//! params; `cleanup()` drains then polls `status.replicas` → 0;
//! `migrate_finalizer()` carries resourceVersion for optimistic lock.
//! The builder tests cover WHAT gets patched; these cover WHEN/HOW.
//!
//! Split from the 1716L monolith (P0396).

// r[verify ctrl.drain.all-then-scale]
// r[verify ctrl.drain.sigterm]

use super::*;

/// apply() hits Service → StatefulSet → WorkerPool/status,
/// server-side apply all three. Wrong order or missing call
/// → verifier panics.
#[tokio::test]
async fn apply_patches_in_order() {
    let (client, verifier) = ApiServerVerifier::new();
    let ctx = test_ctx(client);
    let wp = Arc::new(test_wp());

    // sts_exists=false: first-create path (STS GET → 404 →
    // initial_replicas=Some(min)).
    let guard = verifier.run(apply_ok_scenarios("test-pool", "rio", 2, false));

    // Call apply() directly (not reconcile() — finalizer()
    // would do its own GET + PATCH of metadata.finalizers
    // first, which adds scenarios we don't care about here).
    let action = apply(wp, &ctx).await.expect("apply succeeds");

    // Requeue in 5m — the fallback re-reconcile.
    assert_eq!(action, Action::requeue(Duration::from_secs(300)));

    guard.verified().await;
}

/// Server-side apply params in the query string. SSA is
/// what makes reconcile idempotent — if we accidentally
/// switch to PUT or merge-patch, this fails.
#[tokio::test]
async fn apply_uses_server_side_apply() {
    let (client, verifier) = ApiServerVerifier::new();
    let ctx = test_ctx(client);
    let wp = Arc::new(test_wp());

    // Custom scenarios that assert fieldManager=rio-controller
    // in the query string (SSA puts it there; merge patch
    // doesn't). path_contains is a substring match so
    // embedding the query param works.
    let guard = verifier.run(vec![
        Scenario::ok(
            http::Method::PATCH,
            "fieldManager=rio-controller",
            serde_json::json!({"metadata":{"name":"test-pool-workers"}}).to_string(),
        ),
        // PDB PATCH — same fieldManager assertion.
        Scenario::ok(
            http::Method::PATCH,
            "fieldManager=rio-controller",
            serde_json::json!({"metadata":{"name":"test-pool-pdb"}}).to_string(),
        ),
        // GET before STS PATCH (replicas-ownership check).
        // 404 → first-create → replicas set to min.
        Scenario {
            method: http::Method::GET,
            path_contains: "/statefulsets/test-pool-workers",
            body_contains: None,
            status: 404,
            body_json: serde_json::json!({
                "kind":"Status","apiVersion":"v1",
                "status":"Failure","reason":"NotFound","code":404,
            })
            .to_string(),
        },
        Scenario::ok(
            http::Method::PATCH,
            "fieldManager=rio-controller",
            serde_json::json!({
                "metadata":{"name":"test-pool-workers"},
                "status":{"replicas":0}
            })
            .to_string(),
        ),
        Scenario::ok(
            http::Method::PATCH,
            "workerpools/test-pool/status",
            serde_json::json!({
                "apiVersion":"rio.build/v1alpha1","kind":"WorkerPool",
                "metadata":{"name":"test-pool"},
                "spec":{"replicas":{"min":1,"max":1},"autoscaling":{"metric":"x","targetValue":1},
                    "maxConcurrentBuilds":1,"fuseCacheSize":"1Gi","features":[],
                    "systems":["x"],"sizeClass":"x","image":"x"}
            })
            .to_string(),
        ),
    ]);

    apply(wp, &ctx).await.expect("apply succeeds");
    guard.verified().await;
}

/// cleanup with STS already gone → 404 → short-circuit.
/// Proves the "STS not found → done" branch doesn't hang
/// on the poll loop.
#[tokio::test]
async fn cleanup_tolerates_missing_statefulset() {
    let (client, verifier) = ApiServerVerifier::new();
    let ctx = test_ctx(client);
    let wp = Arc::new(test_wp());

    // Phase 1: pod list (empty — no pods to drain). Phase
    // 2: STS scale PATCH → 404. cleanup() short-circuits.
    // No phase 3 GET.
    let guard = verifier.run(vec![
        // Pod list. Empty — cleanup skips the DrainWorker
        // loop (scheduler unreachable anyway with port 1).
        Scenario::ok(
            http::Method::GET,
            "/pods?&labelSelector=rio.build",
            serde_json::json!({"apiVersion":"v1","kind":"PodList","items":[]}).to_string(),
        ),
        // STS PATCH → 404. K8s 404 body is a Status object.
        Scenario {
            method: http::Method::PATCH,
            path_contains: "/statefulsets/test-pool-workers",
            body_contains: None,
            status: 404,
            body_json: serde_json::json!({
                "apiVersion":"v1","kind":"Status","status":"Failure",
                "reason":"NotFound","code":404,
                "message":"statefulsets.apps \"test-pool-workers\" not found"
            })
            .to_string(),
        },
    ]);

    let action = cleanup(wp, &ctx).await.expect("cleanup tolerates 404");
    assert_eq!(action, Action::await_change());

    guard.verified().await;
}

/// cleanup poll loop: first GET shows replicas=1, second
/// shows 0 → break. Proves we poll `status.replicas` not
/// `readyReplicas` (the latter would see 0 immediately on
/// a terminating pod — wrong).
#[tokio::test(start_paused = true)]
async fn cleanup_polls_until_replicas_zero() {
    let (client, verifier) = ApiServerVerifier::new();
    let ctx = test_ctx(client);
    let wp = Arc::new(test_wp());

    // start_paused + tokio::time means the 5s sleep between
    // polls auto-advances. Without it, this test would take
    // 5 real seconds.
    let sts_with = |replicas: i32| {
        serde_json::json!({
            "apiVersion":"apps/v1","kind":"StatefulSet",
            "metadata":{"name":"test-pool-workers"},
            "status":{"replicas": replicas, "readyReplicas": 0}
        })
        .to_string()
    };

    let guard = verifier.run(vec![
        Scenario::ok(
            http::Method::GET,
            "/pods?&labelSelector=rio.build",
            serde_json::json!({"apiVersion":"v1","kind":"PodList","items":[]}).to_string(),
        ),
        Scenario::ok(
            http::Method::PATCH,
            "/statefulsets/test-pool-workers",
            sts_with(1),
        ),
        // First poll: replicas=1 (pod still terminating).
        // readyReplicas=0 but we DON'T break — proves we
        // read the right field.
        Scenario::ok(
            http::Method::GET,
            "/statefulsets/test-pool-workers",
            sts_with(1),
        ),
        // Second poll: replicas=0. Break.
        Scenario::ok(
            http::Method::GET,
            "/statefulsets/test-pool-workers",
            sts_with(0),
        ),
    ]);

    let action = cleanup(wp, &ctx).await.expect("cleanup completes");
    assert_eq!(action, Action::await_change());

    guard.verified().await;
}

/// migrate_finalizer with stale resourceVersion gets 409, not stomp.
///
/// Scenario: we read finalizers=[OLD], meanwhile a foreign controller
/// adds `example.com/cleanup` (bumping rv). Our merge-patch with the
/// stale resourceVersion MUST be rejected (409 Conflict) and surface
/// as `Error::Conflict` — not succeed and silently drop the foreign
/// finalizer.
///
/// Before the fix: the merge-patch omitted resourceVersion, so the
/// apiserver accepted it unconditionally and set finalizers=[NEW],
/// stomping `example.com/cleanup` that arrived in the window.
///
/// Mutation check: remove `"resourceVersion": rv` from the patch
/// body in migrate_finalizer → the `body_contains` assertion below
/// fails (the mock doesn't see rv in the PATCH body).
#[tokio::test]
async fn migrate_finalizer_conflicts_on_stale_resource_version() {
    let (client, verifier) = ApiServerVerifier::new();
    let api: Api<WorkerPool> = Api::namespaced(client, "rio");

    // The verifier asserts TWO things here:
    //   1. The PATCH body contains `"resourceVersion":"42"` — proves
    //      migrate_finalizer carries the rv it read from `obj`. This
    //      is what makes the apiserver's 409 meaningful (without rv,
    //      merge-patch always succeeds).
    //   2. When the apiserver returns 409, the code maps it to
    //      `Error::Conflict` — proving the error path requeues
    //      instead of bubbling as a generic kube error.
    let guard = verifier.run(vec![Scenario {
        method: http::Method::PATCH,
        path_contains: "/workerpools/test-pool",
        // serde_json emits compact JSON — no space after colon.
        // Asserting the EXACT rv=42 (not just "resourceVersion")
        // proves we read it from obj.meta(), not a hardcoded value.
        body_contains: Some(r#""resourceVersion":"42""#),
        status: 409,
        body_json: serde_json::json!({
            "kind": "Status", "apiVersion": "v1",
            "status": "Failure", "reason": "Conflict", "code": 409,
            "message": "the object has been modified; please apply \
                        your changes to the latest version and try again",
        })
        .to_string(),
    }]);

    // WorkerPool with OLD_FINALIZER + rv=42. This is the STALE
    // snapshot — the "real" apiserver state has rv=43 after a
    // foreign controller added its finalizer.
    let mut wp = test_wp();
    wp.metadata.finalizers = Some(vec![OLD_FINALIZER.into()]);
    wp.metadata.resource_version = Some("42".into());

    let result = migrate_finalizer(&api, &wp, OLD_FINALIZER, FINALIZER).await;

    assert!(
        matches!(result, Err(Error::Conflict(_))),
        "migrate_finalizer should map 409 → Error::Conflict, got {result:?}"
    );

    guard.verified().await;
}

/// Happy path: old finalizer present, rv matches, patch succeeds.
///
/// Proves the no-conflict path still works after adding the
/// resourceVersion lock — the rv is carried in the body AND the
/// 200 response flows through to `Ok(Some(Action::await_change()))`.
#[tokio::test]
async fn migrate_finalizer_happy_path() {
    let (client, verifier) = ApiServerVerifier::new();
    let api: Api<WorkerPool> = Api::namespaced(client, "rio");

    // The mock's 200 stands in for the apiserver accepting the rv.
    // body_contains asserts rv is SENT (same mutation-check
    // coverage as the conflict test).
    let guard = verifier.run(vec![Scenario {
        method: http::Method::PATCH,
        path_contains: "/workerpools/test-pool",
        body_contains: Some(r#""resourceVersion":"7""#),
        status: 200,
        // Response body: the patched WorkerPool. migrate_finalizer
        // doesn't inspect it (just checks for a 200 vs 409), but
        // kube's deserializer needs a valid WorkerPool shape.
        body_json: serde_json::json!({
            "apiVersion": "rio.build/v1alpha1",
            "kind": "WorkerPool",
            "metadata": {
                "name": "test-pool", "namespace": "rio",
                "resourceVersion": "8",
                "finalizers": [FINALIZER],
            },
            "spec": {
                "replicas": { "min": 1, "max": 1 },
                "autoscaling": { "metric": "x", "targetValue": 1 },
                "maxConcurrentBuilds": 1,
                "fuseCacheSize": "1Gi",
                "features": [],
                "systems": ["x86_64-linux"],
                "sizeClass": "small",
                "image": "x",
            },
        })
        .to_string(),
    }]);

    let mut wp = test_wp();
    wp.metadata.finalizers = Some(vec![OLD_FINALIZER.into()]);
    wp.metadata.resource_version = Some("7".into());

    let action = migrate_finalizer(&api, &wp, OLD_FINALIZER, FINALIZER)
        .await
        .expect("no-conflict path succeeds");
    assert_eq!(
        action,
        Some(Action::await_change()),
        "should short-circuit and await the watch event"
    );

    guard.verified().await;
}

/// Old finalizer absent → no patch, return None. Proves the
/// idempotent no-op path doesn't issue any apiserver call (the
/// verifier would time out if it did — zero scenarios).
#[tokio::test]
async fn migrate_finalizer_noop_when_old_absent() {
    let (client, verifier) = ApiServerVerifier::new();
    let api: Api<WorkerPool> = Api::namespaced(client, "rio");

    // No scenarios: migrate_finalizer MUST NOT call the apiserver.
    let guard = verifier.run(vec![]);

    // Only the new finalizer — migration already done.
    let mut wp = test_wp();
    wp.metadata.finalizers = Some(vec![FINALIZER.into()]);
    wp.metadata.resource_version = Some("7".into());

    let action = migrate_finalizer(&api, &wp, OLD_FINALIZER, FINALIZER)
        .await
        .expect("noop path succeeds");
    assert_eq!(action, None, "old absent → None, caller proceeds");

    guard.verified().await;
}
