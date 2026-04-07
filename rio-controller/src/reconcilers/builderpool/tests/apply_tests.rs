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
                "maxConcurrent": 1,
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
