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
use http_body_util::BodyExt;
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
    /// Optional substring the REQUEST body must contain. For
    /// asserting that the code under test sent a specific field
    /// (e.g., `"resourceVersion"` in a merge-patch). None = no
    /// body assertion (most tests don't care).
    pub body_contains: Option<&'static str>,
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
            body_contains: None,
            status: 200,
            body_json,
        }
    }

    /// Shorthand: K8s error-Status response. The body follows the
    /// `metav1.Status` envelope that `kube::Error::Api` deserializes
    /// from — `reason` maps to `ErrorResponse.reason`, `code` to
    /// `.code`, `message` to `.message`. Test code typically matches
    /// on `kube::Error::Api(ae) if ae.code == <N>`, so get the code
    /// right; reason/message are diagnostic.
    ///
    /// ```
    /// # use rio_test_support::kube_mock::Scenario;
    /// # use http::Method;
    /// let forbidden = Scenario::k8s_error(
    ///     Method::POST, "/namespaces/rio/jobs",
    ///     403, "Forbidden", "jobs.batch is forbidden: exceeded quota",
    /// );
    /// ```
    pub fn k8s_error(
        method: http::Method,
        path_contains: &'static str,
        code: u16,
        reason: &'static str,
        message: &'static str,
    ) -> Self {
        Self {
            method,
            path_contains,
            body_contains: None,
            status: code,
            body_json: serde_json::json!({
                "kind": "Status",
                "apiVersion": "v1",
                "status": "Failure",
                "reason": reason,
                "code": code,
                "message": message,
            })
            .to_string(),
        }
    }
}

/// Returned by [`ApiServerVerifier::run`]. Holds the verifier task handle;
/// panics on drop if [`VerifierGuard::verified`] wasn't called.
///
/// Why a drop-bomb instead of just `#[must_use]` on the handle: a test
/// that binds `let _task = verifier.run(...)` defeats `#[must_use]` but
/// still forgets to join. The bomb catches both: unbound (`#[must_use]`
/// compile warning) AND bound-but-unjoined (runtime panic).
///
/// Disarming via `ManuallyDrop` is deliberately not exposed — the only
/// way to disarm is to actually verify.
#[must_use = "call .verified().await or the verifier panics on drop"]
pub struct VerifierGuard {
    handle: JoinHandle<()>,
    armed: bool,
}

impl VerifierGuard {
    /// Join the verifier task under a 5 s timeout. Returns after all
    /// scenarios are processed (always `scenarios.len()` on success —
    /// the assert is inside the spawned task, so any mismatch already
    /// panicked before this point).
    ///
    /// The 5 s timeout catches code-under-test that made FEWER calls
    /// than scenarios (verifier blocks on `next_request()` forever).
    /// 5 s is well above any reconcile/election tick.
    pub async fn verified(mut self) {
        self.armed = false;
        tokio::time::timeout(std::time::Duration::from_secs(5), &mut self.handle)
            .await
            .expect("verifier consumed all scenarios (code made the expected number of calls)")
            .expect("verifier assertions passed (method/path matched every scenario)");
    }
}

impl Drop for VerifierGuard {
    fn drop(&mut self) {
        if self.armed && !std::thread::panicking() {
            panic!(
                "VerifierGuard dropped without .verified().await — \
                 test never proved the code made the expected HTTP calls"
            );
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
    pub fn run(self, scenarios: Vec<Scenario>) -> VerifierGuard {
        let handle = tokio::spawn(async move {
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

                if let Some(want) = scenario.body_contains {
                    // Collect the request body to assert on its
                    // content. kube::client::Body is a hyper-style
                    // stream; BodyExt::collect drains it to bytes.
                    let bytes = request
                        .into_body()
                        .collect()
                        .await
                        .expect("request body collectible")
                        .to_bytes();
                    // kube only emits UTF-8 JSON — from_utf8 never
                    // fails here, and a surprise non-UTF-8 body is
                    // worth loud panic (clippy disallows the lossy
                    // variant workspace-wide to catch parse paths).
                    let body =
                        std::str::from_utf8(&bytes).expect("kube request body is UTF-8 JSON");
                    assert!(
                        body.contains(want),
                        "scenario {i}: request body {body:?} doesn't contain {want:?}"
                    );
                }

                send.send_response(
                    Response::builder()
                        .status(scenario.status)
                        .header("content-type", "application/json")
                        .body(Body::from(scenario.body_json.into_bytes()))
                        .expect("valid response"),
                );
            }
        });
        VerifierGuard {
            handle,
            armed: true,
        }
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
        let guard = verifier.run(vec![Scenario::ok(
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

        guard.verified().await;
    }
}
