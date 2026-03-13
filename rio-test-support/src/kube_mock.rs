//! tower-test fixtures for kube::Client integration tests.
//!
//! Wraps kube's `Client::new(mock_service, ns)` pattern in a
//! scenario-driven verifier: the test spells out what HTTP
//! requests it EXPECTS the code under test to make (in order),
//! and what to respond with. The verifier task drains requests
//! one at a time, asserting method/path, sending canned JSON
//! bodies.
//!
//! Extracted from rio-controller so rio-scheduler's lease
//! election tests can share the same mock-apiserver plumbing.

use std::pin::pin;

use http::{Request, Response};
use kube::Client;
use kube::client::Body;
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
    /// the code under test; the verifier's `run()` is spawned
    /// and joined after the call returns.
    pub fn new() -> (Client, Self) {
        let (mock_service, handle) = mock::pair::<Request<Body>, Response<Body>>();
        // "default" namespace: tests' fixture resources use an
        // explicit namespace, so this default is never actually
        // used (Api::namespaced overrides it). Any string works.
        let client = Client::new(mock_service, "default");
        (client, Self { handle })
    }

    /// Spawn a task that processes scenarios in order. Each
    /// scenario blocks until the NEXT request arrives, asserts
    /// method/path, sends the canned response. When scenarios
    /// are exhausted, the task returns — any further request
    /// hangs (the test's outer timeout catches that).
    ///
    /// Join this handle AFTER the call under test to prove all
    /// scenarios were consumed (code made exactly the expected
    /// calls, no more, no less). Use a timeout — if the code
    /// made FEWER calls, the verifier blocks on next_request()
    /// forever.
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

#[cfg(test)]
mod tests {
    use super::*;
    use k8s_openapi::api::core::v1::Pod;
    use kube::Api;

    /// Sanity: mock client round-trips a single GET. Proves the
    /// tower-test + kube::Client plumbing works before layering
    /// reconciler/election logic on top.
    #[tokio::test]
    async fn verifier_roundtrip() {
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
}
