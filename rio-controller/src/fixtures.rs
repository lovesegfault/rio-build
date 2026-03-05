//! tower-test fixtures for reconciler integration tests.
//!
//! Wraps kube's `Client::new(mock_service, ns)` pattern in a
//! scenario-driven verifier: the test spells out what HTTP
//! requests it EXPECTS the reconciler to make (in order), and
//! what to respond with. The verifier task drains requests one
//! at a time, asserting method/path, sending canned JSON bodies.
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
//! cleanup() are one RPC (DrainWorker) and F5's apply does two
//! (connect + SubmitBuild). A MockScheduler is a separate thing
//! (rio-test-support already has one, but not wired for
//! AdminService). Scope: K8s mocks only. Scheduler integration
//! is what vm-phase3a (H2) is for.

#![cfg(test)]

use std::pin::pin;

use http::{Request, Response};
use kube::client::Body;
use kube::{Api, Client};
use tokio::task::JoinHandle;
use tower_test::mock::{self, Handle};

/// One expected HTTP interaction. The verifier asserts the
/// incoming request matches `method` + `path_contains`, then
/// responds with `status` + `body_json`.
///
/// `path_contains` not exact match: K8s paths have query params
/// (`?fieldManager=rio-controller&force=true`) that are noisy to
/// assert exactly. Substring match on the RESOURCE part of the
/// path is enough to prove "we hit the right endpoint."
pub struct Scenario {
    pub method: http::Method,
    /// Substring the path must contain. E.g., "/statefulsets/
    /// test-pool-workers" — matches regardless of query params.
    pub path_contains: &'static str,
    /// Response status. 200 for happy path, 404 for "not found."
    pub status: u16,
    /// Response body as JSON string. Use `r#"..."#` for literal
    /// JSON, or `serde_json::to_string(&obj)` for typed.
    pub body_json: String,
}

impl Scenario {
    /// Shorthand: 200 OK with the given body.
    pub fn ok(method: http::Method, path_contains: &'static str, body_json: String) -> Self {
        Self {
            method,
            path_contains,
            status: 200,
            body_json,
        }
    }
}

/// Wraps the tower-test Handle. `run()` spawns a task that
/// processes scenarios in order until exhausted.
pub struct ApiServerVerifier {
    handle: Handle<Request<Body>, Response<Body>>,
}

impl ApiServerVerifier {
    /// Create a mock Client + verifier. The Client is fed into
    /// `Ctx`; the verifier's `run()` is spawned and joined after
    /// the reconcile call.
    pub fn new() -> (Client, Self) {
        let (mock_service, handle) = mock::pair::<Request<Body>, Response<Body>>();
        // "default" namespace: tests' fixture WorkerPool uses
        // namespace="rio" explicitly, so this default is never
        // actually used (Api::namespaced overrides it). Any
        // string works.
        let client = Client::new(mock_service, "default");
        (client, Self { handle })
    }

    /// Spawn a task that processes scenarios in order. Each
    /// scenario blocks until the NEXT request arrives, asserts
    /// method/path, sends the canned response. When scenarios
    /// are exhausted, the task returns — any further request
    /// hangs (the test's outer timeout catches that).
    ///
    /// Join this handle AFTER the reconcile future to prove all
    /// scenarios were consumed (reconciler made exactly the
    /// expected calls, no more, no less). Use a timeout — if
    /// reconcile made FEWER calls, the verifier blocks on
    /// next_request() forever.
    pub fn run(self, scenarios: Vec<Scenario>) -> JoinHandle<()> {
        tokio::spawn(async move {
            let mut handle = pin!(self.handle);
            for (i, scenario) in scenarios.into_iter().enumerate() {
                let (request, send) = handle
                    .next_request()
                    .await
                    .unwrap_or_else(|| panic!("scenario {i}: client dropped before request"));

                let got_method = request.method().clone();
                let got_path = request.uri().to_string();
                assert_eq!(
                    got_method, scenario.method,
                    "scenario {i}: method mismatch. path was: {got_path}"
                );
                assert!(
                    got_path.contains(scenario.path_contains),
                    "scenario {i}: path {got_path:?} doesn't contain {:?}",
                    scenario.path_contains
                );

                send.send_response(
                    Response::builder()
                        .status(scenario.status)
                        .header("content-type", "application/json")
                        .body(Body::from(scenario.body_json.into_bytes()))
                        .expect("valid response"),
                );
            }
        })
    }
}

/// Convenience: a "do-nothing-extra" scenario list for apply().
/// Service PATCH → StatefulSet PATCH → status PATCH, all 200.
/// Use when testing "apply succeeds" without caring about the
/// specific patch bodies (those are covered by the pure builder
/// tests).
///
/// The STATEFULSET response body matters: apply() reads
/// `.status.replicas` from it to patch WorkerPool.status.
/// Service + status responses are ignored.
pub fn apply_ok_scenarios(pool_name: &str, ns: &str, sts_replicas: i32) -> Vec<Scenario> {
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
    vec![
        Scenario::ok(
            http::Method::PATCH,
            // Can't use format! in a const context, and Scenario
            // takes &'static str. Leak — fine for tests, the
            // process ends before the heap is reclaimed anyway.
            Box::leak(format!("/services/{sts_name}").into_boxed_str()),
            svc_body.to_string(),
        ),
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

/// Sanity: mock client round-trips a single GET. Proves the
/// tower-test + kube::Client plumbing works before we layer
/// reconciler logic on top.
#[tokio::test]
async fn verifier_roundtrip() {
    use k8s_openapi::api::core::v1::Pod;

    let (client, verifier) = ApiServerVerifier::new();
    let task = verifier.run(vec![Scenario::ok(
        http::Method::GET,
        "/pods/test-pod",
        serde_json::json!({
            "apiVersion": "v1",
            "kind": "Pod",
            "metadata": { "name": "test-pod", "namespace": "default" },
        })
        .to_string(),
    )]);

    let api: Api<Pod> = Api::default_namespaced(client);
    let pod = api.get("test-pod").await.expect("mock returns pod");
    assert_eq!(pod.metadata.name.as_deref(), Some("test-pod"));

    tokio::time::timeout(std::time::Duration::from_secs(1), task)
        .await
        .expect("verifier completed")
        .expect("verifier didn't panic");
}
