//! Reconciler integration test fixtures.
//!
//! The generic scenario-driven mock apiserver lives in
//! `rio-test-support::kube_mock` (shared with rio-scheduler's
//! lease election tests). This module keeps the rio-controller-
//! specific scenario builders that know about WorkerPool/
//! StatefulSet/Service/PDB shapes.
//!
//! This tests the WIRING, not the business logic — that's what
//! the pure unit tests on `build_statefulset` etc are for. Here
//! we prove: apply() patches Service then StatefulSet then
//! WorkerPool/status, in that order, with server-side-apply
//! params. Get the order wrong → test fails.
//!
//! # Why not mock the WHOLE thing (scheduler too)
//!
//! Reconcile's K8s calls are the meat — that's what finalizer()
//! and server-side apply make tricky. The scheduler calls in
//! cleanup() are one RPC (DrainWorker). A MockScheduler is a
//! separate thing (rio-test-support already has one, but not
//! wired for AdminService). Scope: K8s mocks only. Scheduler
//! integration is what vm-phase3a is for.
//!
//! `#[cfg(test)]` is on the `mod fixtures;` in lib.rs — not
//! here (stable clippy flags the duplicate).

pub use rio_test_support::kube_mock::{ApiServerVerifier, Scenario};

/// Convenience: a "do-nothing-extra" scenario list for apply().
/// Service PATCH → StatefulSet PATCH → status PATCH, all 200.
/// Use when testing "apply succeeds" without caring about the
/// specific patch bodies (those are covered by the pure builder
/// tests).
///
/// The STATEFULSET response body matters: apply() reads
/// `.status.replicas` from it to patch WorkerPool.status.
/// Service + status responses are ignored.
///
/// `sts_exists`: whether the STS GET (before PATCH) returns 200
/// or 404. apply() uses this to decide whether to set
/// spec.replicas (first-create) or omit it (autoscaler owns it).
pub fn apply_ok_scenarios(
    pool_name: &str,
    ns: &str,
    sts_replicas: i32,
    sts_exists: bool,
) -> Vec<Scenario> {
    let sts_name = format!("{pool_name}-workers");
    // Minimal Service: apply() doesn't read anything from the
    // response. Empty-ish JSON that parses as a Service.
    let svc_body = serde_json::json!({
        "apiVersion": "v1",
        "kind": "Service",
        "metadata": { "name": sts_name, "namespace": ns },
    });
    // StatefulSet WITH status: apply() reads
    // applied.status.replicas + ready_replicas.
    let sts_body = serde_json::json!({
        "apiVersion": "apps/v1",
        "kind": "StatefulSet",
        "metadata": { "name": sts_name, "namespace": ns },
        "spec": { "replicas": sts_replicas },
        "status": { "replicas": sts_replicas, "readyReplicas": sts_replicas },
    });
    // GET response: 404 "not found" or the same sts_body for 200.
    // apply() uses get_opt which maps 404 → None → first-create.
    let sts_get = if sts_exists {
        Scenario::ok(
            http::Method::GET,
            Box::leak(format!("/statefulsets/{sts_name}").into_boxed_str()),
            sts_body.to_string(),
        )
    } else {
        Scenario {
            method: http::Method::GET,
            path_contains: Box::leak(format!("/statefulsets/{sts_name}").into_boxed_str()),
            status: 404,
            // kube's get_opt parses the 404 body as a Status, not a
            // StatefulSet. Standard K8s NotFound shape.
            body_json: serde_json::json!({
                "kind": "Status", "apiVersion": "v1",
                "status": "Failure", "reason": "NotFound", "code": 404,
            })
            .to_string(),
        }
    };
    // WorkerPool status patch response: also ignored by apply().
    // Needs at least valid metadata + spec for the serde
    // round-trip in kube's response decode.
    let wp_body = serde_json::json!({
        "apiVersion": "rio.build/v1alpha1",
        "kind": "WorkerPool",
        "metadata": { "name": pool_name, "namespace": ns },
        "spec": {
            "replicas": { "min": 1, "max": 1 },
            "autoscaling": { "metric": "queueDepth", "targetValue": 1 },
            "maxConcurrentBuilds": 1,
            "fuseCacheSize": "1Gi",
            "features": [],
            "systems": ["x86_64-linux"],
            "sizeClass": "small",
            "image": "x",
        },
    });
    // PDB response: ignored like Service. Minimal shape that parses.
    let pdb_body = serde_json::json!({
        "apiVersion": "policy/v1",
        "kind": "PodDisruptionBudget",
        "metadata": { "name": format!("{pool_name}-pdb"), "namespace": ns },
    });
    vec![
        Scenario::ok(
            http::Method::PATCH,
            // Can't use format! in a const context, and Scenario
            // takes &'static str. Leak — fine for tests, the
            // process ends before the heap is reclaimed anyway.
            Box::leak(format!("/services/{sts_name}").into_boxed_str()),
            svc_body.to_string(),
        ),
        // PDB PATCH — after Service, before STS (apply() order).
        Scenario::ok(
            http::Method::PATCH,
            Box::leak(format!("/poddisruptionbudgets/{pool_name}-pdb").into_boxed_str()),
            pdb_body.to_string(),
        ),
        sts_get,
        Scenario::ok(
            http::Method::PATCH,
            Box::leak(format!("/statefulsets/{sts_name}").into_boxed_str()),
            sts_body.to_string(),
        ),
        Scenario::ok(
            http::Method::PATCH,
            Box::leak(format!("/workerpools/{pool_name}/status").into_boxed_str()),
            wp_body.to_string(),
        ),
    ]
}
