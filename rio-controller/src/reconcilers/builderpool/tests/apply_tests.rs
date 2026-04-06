//! Mock-apiserver reconcile-loop wiring tests.
//!
//! These test the WIRING: `apply()` calls Service PATCH → StatefulSet
//! PATCH → BuilderPool/status PATCH in that order with server-side-apply
//! params; `cleanup()` drains then polls `status.replicas` → 0;
//! `migrate_finalizer()` carries resourceVersion for optimistic lock.
//! The builder tests cover WHAT gets patched; these cover WHEN/HOW.
//!
//! Split from the 1716L monolith (P0396).

// r[verify ctrl.drain.all-then-scale]
// r[verify ctrl.drain.sigterm]

use super::*;

/// apply() hits Service → StatefulSet → BuilderPool/status,
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
            serde_json::json!({"metadata":{"name":"test-pool-builder"}}).to_string(),
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
            path_contains: "/statefulsets/test-pool-builder",
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
                "metadata":{"name":"test-pool-builder"},
                "status":{"replicas":0}
            })
            .to_string(),
        ),
        Scenario::ok(
            http::Method::PATCH,
            "builderpools/test-pool/status",
            serde_json::json!({
                "apiVersion":"rio.build/v1alpha1","kind":"BuilderPool",
                "metadata":{"name":"test-pool"},
                "spec":{"replicas":{"min":1,"max":1},"autoscaling":{"metric":"x","targetValue":1},
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
        // Pod list. Empty — cleanup skips the DrainExecutor
        // loop (scheduler unreachable anyway with port 1).
        Scenario::ok(
            http::Method::GET,
            "/pods?&labelSelector=rio.build",
            serde_json::json!({"apiVersion":"v1","kind":"PodList","items":[]}).to_string(),
        ),
        // STS PATCH → 404. K8s 404 body is a Status object.
        Scenario {
            method: http::Method::PATCH,
            path_contains: "/statefulsets/test-pool-builder",
            body_contains: None,
            status: 404,
            body_json: serde_json::json!({
                "apiVersion":"v1","kind":"Status","status":"Failure",
                "reason":"NotFound","code":404,
                "message":"statefulsets.apps \"test-pool-builder\" not found"
            })
            .to_string(),
        },
    ]);

    let action = cleanup(wp, &ctx).await.expect("cleanup tolerates 404");
    assert_eq!(action, Action::await_change());

    guard.verified().await;
}

/// cleanup drain check: replicas=1 → requeue (NOT
/// await_change). Proves we read `status.replicas` not
/// `readyReplicas` (the latter would see 0 immediately on
/// a terminating pod — wrong), and that a still-draining
/// pool REQUEUES instead of blocking inline.
///
/// The requeue is the load-bearing behavior: an inline sleep
/// loop would monopolize the reconcile slot for up to 2h+
/// (terminationGracePeriodSeconds). Requeue lets other pools
/// reconcile in between.
#[tokio::test]
async fn cleanup_requeues_while_draining() {
    let (client, verifier) = ApiServerVerifier::new();
    let ctx = test_ctx(client);
    // deletionTimestamp = now → elapsed ≈ 0, well within the
    // default 2h+60s grace. cleanup() sees replicas>0 and
    // returns requeue, NOT await_change.
    let mut wp = test_wp();
    wp.metadata.deletion_timestamp = Some(k8s_openapi::apimachinery::pkg::apis::meta::v1::Time(
        k8s_openapi::jiff::Timestamp::now(),
    ));
    let wp = Arc::new(wp);

    let sts_with = |replicas: i32| {
        serde_json::json!({
            "apiVersion":"apps/v1","kind":"StatefulSet",
            "metadata":{"name":"test-pool-builder"},
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
            "/statefulsets/test-pool-builder",
            sts_with(1),
        ),
        // Single GET: replicas=1 (pod still terminating).
        // readyReplicas=0 but we DON'T return await_change —
        // proves we read the right field AND that we requeue
        // instead of looping inline.
        Scenario::ok(
            http::Method::GET,
            "/statefulsets/test-pool-builder",
            sts_with(1),
        ),
    ]);

    let action = cleanup(wp, &ctx).await.expect("cleanup returns");
    // 5s requeue = DRAIN_POLL_INTERVAL. The finalizer stays;
    // next Event::Cleanup tick re-enters cleanup().
    assert_eq!(
        action,
        Action::requeue(Duration::from_secs(5)),
        "still-draining pool must requeue, not block inline or remove finalizer"
    );

    guard.verified().await;
}

/// cleanup drain check: replicas=0 → await_change (finalizer
/// removed). The terminal state after however-many requeues.
#[tokio::test]
async fn cleanup_completes_when_replicas_zero() {
    let (client, verifier) = ApiServerVerifier::new();
    let ctx = test_ctx(client);
    let wp = Arc::new(test_wp());

    let sts_with = |replicas: i32| {
        serde_json::json!({
            "apiVersion":"apps/v1","kind":"StatefulSet",
            "metadata":{"name":"test-pool-builder"},
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
            "/statefulsets/test-pool-builder",
            sts_with(0),
        ),
        Scenario::ok(
            http::Method::GET,
            "/statefulsets/test-pool-builder",
            sts_with(0),
        ),
    ]);

    let action = cleanup(wp, &ctx).await.expect("cleanup completes");
    assert_eq!(action, Action::await_change());

    guard.verified().await;
}

/// Drain timeout: deletionTimestamp far in the past (beyond
/// grace + slop) → proceed even with replicas>0. Proves the
/// deadline is computed from the apiserver's deletionTimestamp
/// (stable across requeues) not a per-call Instant (which
/// would reset every tick and never trip).
#[tokio::test]
async fn cleanup_times_out_from_deletion_timestamp() {
    let (client, verifier) = ApiServerVerifier::new();
    let ctx = test_ctx(client);
    // Short grace (10s) so the timeout trips without faking
    // 2h of wall clock. deletionTimestamp = now - 100s puts
    // us past grace(10s) + slop(60s) = 70s.
    let mut wp = test_wp();
    wp.spec.termination_grace_period_seconds = Some(10);
    let past =
        k8s_openapi::jiff::Timestamp::now() - k8s_openapi::jiff::SignedDuration::from_secs(100);
    wp.metadata.deletion_timestamp =
        Some(k8s_openapi::apimachinery::pkg::apis::meta::v1::Time(past));
    let wp = Arc::new(wp);

    let sts_with = |replicas: i32| {
        serde_json::json!({
            "apiVersion":"apps/v1","kind":"StatefulSet",
            "metadata":{"name":"test-pool-builder"},
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
            "/statefulsets/test-pool-builder",
            sts_with(2),
        ),
        // replicas=2 (stuck pods) but deadline has passed.
        // cleanup() proceeds anyway — ownerRef GC will
        // force-delete.
        Scenario::ok(
            http::Method::GET,
            "/statefulsets/test-pool-builder",
            sts_with(2),
        ),
    ]);

    let action = cleanup(wp, &ctx).await.expect("cleanup times out");
    assert_eq!(
        action,
        Action::await_change(),
        "past-deadline drain must proceed (remove finalizer), not requeue forever"
    );

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
    let api: Api<BuilderPool> = Api::namespaced(client, "rio");

    // The verifier asserts TWO things here:
    //   1. The PATCH body contains `"resourceVersion":"42"` — proves
    //      migrate_finalizer carries the rv it read from `obj`. This
    //      is what makes the apiserver's 409 meaningful (without rv,
    //      merge-patch always succeeds).
    //   2. When the apiserver returns 409, the code maps it to
    //      `Error::Conflict` — proving the error path requeues
    //      instead of bubbling as a generic kube error.
    let guard = verifier.run(vec![Scenario {
        // serde_json emits compact JSON — no space after colon.
        // Asserting the EXACT rv=42 (not just "resourceVersion")
        // proves we read it from obj.meta(), not a hardcoded value.
        body_contains: Some(r#""resourceVersion":"42""#),
        ..Scenario::k8s_error(
            http::Method::PATCH,
            "/builderpools/test-pool",
            409,
            "Conflict",
            "the object has been modified; please apply \
             your changes to the latest version and try again",
        )
    }]);

    // BuilderPool with OLD_FINALIZER + rv=42. This is the STALE
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
    let api: Api<BuilderPool> = Api::namespaced(client, "rio");

    // The mock's 200 stands in for the apiserver accepting the rv.
    // body_contains asserts rv is SENT (same mutation-check
    // coverage as the conflict test).
    let guard = verifier.run(vec![Scenario {
        method: http::Method::PATCH,
        path_contains: "/builderpools/test-pool",
        body_contains: Some(r#""resourceVersion":"7""#),
        status: 200,
        // Response body: the patched BuilderPool. migrate_finalizer
        // doesn't inspect it (just checks for a 200 vs 409), but
        // kube's deserializer needs a valid BuilderPool shape.
        body_json: serde_json::json!({
            "apiVersion": "rio.build/v1alpha1",
            "kind": "BuilderPool",
            "metadata": {
                "name": "test-pool", "namespace": "rio",
                "resourceVersion": "8",
                "finalizers": [FINALIZER],
            },
            "spec": {
                "replicas": { "min": 1, "max": 1 },
                "autoscaling": { "metric": "x", "targetValue": 1 },
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
    let api: Api<BuilderPool> = Api::namespaced(client, "rio");

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
