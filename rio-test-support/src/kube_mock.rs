//! tower-test fixtures for kube::Client integration tests.
//!
//! Two mocks with opposite philosophies:
//!
//! - [`ApiServerVerifier`] is a *scripted scenario queue*: the test
//!   spells out what HTTP requests it EXPECTS the code under test to
//!   make (in order) and what to respond with. The right tool for
//!   "assert the client sent exactly these requests".
//! - [`MockApiServer`] is a *stateful fake*: an in-memory Lease store
//!   with real optimistic-concurrency semantics that serves any request
//!   sequence correctly. The right tool for interleaved multi-client
//!   races and model-based tests, where the request sequence is
//!   generated rather than hand-planned.
//!
//! Extracted from rio-controller so rio-scheduler's lease
//! election tests can share the same mock-apiserver plumbing.

use std::pin::pin;
use std::sync::{Arc, Mutex};

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
    // r[impl ts.kube.verifier-guard]
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
// r[impl ts.kube.verifier-guard]
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

/// A stateful in-memory mock of the `coordination.k8s.io` Lease API
/// with real optimistic-concurrency semantics.
///
/// One lease, dispatched purely on HTTP method (the only object the
/// lease-election code touches):
///
/// | Method | Behavior |
/// |---|---|
/// | GET    | 200 with the stored object, or a 404 Status. |
/// | POST   | 409 if one exists, else store with `resourceVersion: "1"`. |
/// | PUT    | 409 unless the submitted `metadata.resourceVersion` equals the stored one (**the CAS**), else store with the rv bumped. |
/// | DELETE | clear the stored object; 200 if one existed, 404 otherwise. |
///
/// The handler task runs until the [`Client`] is dropped. The returned
/// `MockApiServer` handle shares the stored state for inspection
/// (assertions on who won a race) and out-of-band manipulation (an
/// operator's `kubectl delete lease`, which is not something the code
/// under test performs).
pub struct MockApiServer {
    state: Arc<Mutex<Option<serde_json::Value>>>,
}

impl MockApiServer {
    /// Create a mock Client backed by a fresh, empty Lease store and
    /// spawn the handler task. The task exits when the Client (and
    /// every clone of it) is dropped.
    pub fn new() -> (Client, Self) {
        let (mock_service, handle) = mock::pair::<Request<Body>, Response<Body>>();
        let client = Client::new(mock_service, "default");
        let state: Arc<Mutex<Option<serde_json::Value>>> = Arc::new(Mutex::new(None));
        let task_state = Arc::clone(&state);
        tokio::spawn(async move {
            let mut handle = pin!(handle);
            while let Some((request, send)) = handle.next_request().await {
                let method = request.method().clone();
                let body_bytes = request
                    .into_body()
                    .collect()
                    .await
                    .expect("request body collectible")
                    .to_bytes();
                let response = Self::handle(&task_state, &method, &body_bytes);
                send.send_response(response);
            }
        });
        (client, Self { state })
    }

    /// Dispatch one request against the stored state. Synchronous so the
    /// mutex is never held across an await.
    fn handle(
        state: &Mutex<Option<serde_json::Value>>,
        method: &http::Method,
        body: &[u8],
    ) -> Response<Body> {
        let mut stored = state.lock().expect("mock apiserver state lock");
        match *method {
            http::Method::GET => match stored.as_ref() {
                Some(obj) => Self::json(200, obj.to_string()),
                None => Self::status(404, "NotFound", "lease not found"),
            },
            http::Method::POST => {
                if stored.is_some() {
                    return Self::status(409, "AlreadyExists", "lease already exists");
                }
                let mut obj: serde_json::Value =
                    serde_json::from_slice(body).expect("POST body is a JSON Lease");
                obj["metadata"]["resourceVersion"] = serde_json::Value::String("1".into());
                let response = Self::json(201, obj.to_string());
                *stored = Some(obj);
                response
            }
            http::Method::PUT => {
                let Some(current) = stored.as_ref() else {
                    return Self::status(404, "NotFound", "lease not found");
                };
                let submitted: serde_json::Value =
                    serde_json::from_slice(body).expect("PUT body is a JSON Lease");
                // THE CAS: the apiserver admits the write only if the
                // submitted resourceVersion matches the stored one.
                let stored_rv = current["metadata"]["resourceVersion"]
                    .as_str()
                    .expect("stored lease has a resourceVersion");
                let submitted_rv = submitted["metadata"]["resourceVersion"].as_str();
                if submitted_rv != Some(stored_rv) {
                    return Self::status(
                        409,
                        "Conflict",
                        "the object has been modified; please apply your changes to the \
                         latest version and try again",
                    );
                }
                let next_rv = stored_rv
                    .parse::<u64>()
                    .expect("mock-assigned resourceVersion is numeric")
                    + 1;
                let mut obj = submitted;
                obj["metadata"]["resourceVersion"] = serde_json::Value::String(next_rv.to_string());
                let response = Self::json(200, obj.to_string());
                *stored = Some(obj);
                response
            }
            http::Method::DELETE => {
                if stored.take().is_some() {
                    Self::status(200, "Success", "lease deleted")
                } else {
                    Self::status(404, "NotFound", "lease not found")
                }
            }
            ref other => Self::status(405, "MethodNotAllowed", &format!("{other} not handled")),
        }
    }

    fn json(status: u16, body: String) -> Response<Body> {
        Response::builder()
            .status(status)
            .header("content-type", "application/json")
            .body(Body::from(body.into_bytes()))
            .expect("valid response")
    }

    /// A `metav1.Status` envelope — what `kube::Error::Api` deserializes
    /// from. `code` is what `is_conflict()` / `get_opt()`'s 404 handling
    /// actually match on.
    fn status(code: u16, reason: &str, message: &str) -> Response<Body> {
        Self::json(
            code,
            serde_json::json!({
                "kind": "Status",
                "apiVersion": "v1",
                "status": "Failure",
                "reason": reason,
                "code": code,
                "message": message,
            })
            .to_string(),
        )
    }

    /// The currently stored Lease object, if any. For test assertions on
    /// who won a race.
    pub fn stored(&self) -> Option<serde_json::Value> {
        self.state
            .lock()
            .expect("mock apiserver state lock")
            .clone()
    }

    /// The stored object's `spec.holderIdentity`, if a lease exists and
    /// the field is set.
    pub fn holder(&self) -> Option<String> {
        self.stored()
            .and_then(|o| o["spec"]["holderIdentity"].as_str().map(str::to_owned))
    }

    /// The stored object's `metadata.resourceVersion`, if a lease exists.
    pub fn resource_version(&self) -> Option<String> {
        self.stored()
            .and_then(|o| o["metadata"]["resourceVersion"].as_str().map(str::to_owned))
    }

    /// Pre-populate the store with a lease (e.g. one held by a dead
    /// third party, so two live replicas can race to steal it). The
    /// caller supplies the full JSON object including
    /// `metadata.resourceVersion`.
    pub fn seed(&self, lease: serde_json::Value) {
        *self.state.lock().expect("mock apiserver state lock") = Some(lease);
    }

    /// Clear the stored lease out of band — an operator's
    /// `kubectl delete lease`, which the code under test never performs
    /// itself.
    pub fn clear(&self) -> bool {
        self.state
            .lock()
            .expect("mock apiserver state lock")
            .take()
            .is_some()
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

    // ---- MockApiServer: the optimistic-concurrency contract --------

    use k8s_openapi::api::coordination::v1::{Lease, LeaseSpec};
    use kube::api::{ObjectMeta, PostParams};

    fn lease(name: &str, holder: &str, rv: Option<&str>) -> Lease {
        Lease {
            metadata: ObjectMeta {
                name: Some(name.into()),
                resource_version: rv.map(str::to_owned),
                ..Default::default()
            },
            spec: Some(LeaseSpec {
                holder_identity: Some(holder.into()),
                ..Default::default()
            }),
        }
    }

    fn is_conflict(e: &kube::Error) -> bool {
        matches!(e, kube::Error::Api(ae) if ae.code == 409)
    }

    /// POST creates exactly once: the second creator gets 409 and the
    /// stored object is the first creator's.
    #[tokio::test]
    async fn mock_create_then_create_conflicts() {
        let (client, mock) = MockApiServer::new();
        let api: Api<Lease> = Api::default_namespaced(client);

        api.create(&PostParams::default(), &lease("l", "n1", None))
            .await
            .expect("first create succeeds");
        assert_eq!(mock.holder().as_deref(), Some("n1"));
        assert_eq!(mock.resource_version().as_deref(), Some("1"));

        let err = api
            .create(&PostParams::default(), &lease("l", "n2", None))
            .await
            .expect_err("second create conflicts");
        assert!(is_conflict(&err), "expected 409, got {err:?}");
        assert_eq!(
            mock.holder().as_deref(),
            Some("n1"),
            "loser did not clobber"
        );
    }

    /// GET → PUT at the fetched resourceVersion succeeds and bumps the
    /// rv; the stored object is the submitted one.
    #[tokio::test]
    async fn mock_replace_at_current_rv_succeeds_and_bumps() {
        let (client, mock) = MockApiServer::new();
        let api: Api<Lease> = Api::default_namespaced(client);
        api.create(&PostParams::default(), &lease("l", "n1", None))
            .await
            .expect("create");

        let fetched = api.get("l").await.expect("get");
        let rv = fetched.metadata.resource_version.clone();
        assert_eq!(rv.as_deref(), Some("1"));

        api.replace(
            "l",
            &PostParams::default(),
            &lease("l", "n2", rv.as_deref()),
        )
        .await
        .expect("replace at the fetched rv succeeds");
        assert_eq!(mock.holder().as_deref(), Some("n2"));
        assert_eq!(mock.resource_version().as_deref(), Some("2"), "rv bumped");
    }

    /// PUT at a stale resourceVersion gets 409 and the stored object is
    /// untouched — THE CAS.
    #[tokio::test]
    async fn mock_replace_at_stale_rv_conflicts_and_preserves() {
        let (client, mock) = MockApiServer::new();
        let api: Api<Lease> = Api::default_namespaced(client);
        api.create(&PostParams::default(), &lease("l", "n1", None))
            .await
            .expect("create");
        // Someone else writes: rv 1 → 2.
        api.replace("l", &PostParams::default(), &lease("l", "n1", Some("1")))
            .await
            .expect("first replace");
        assert_eq!(mock.resource_version().as_deref(), Some("2"));

        // A write still carrying rv 1 must bounce.
        let err = api
            .replace("l", &PostParams::default(), &lease("l", "thief", Some("1")))
            .await
            .expect_err("stale rv conflicts");
        assert!(is_conflict(&err), "expected 409, got {err:?}");
        assert_eq!(
            mock.holder().as_deref(),
            Some("n1"),
            "loser did not clobber"
        );
        assert_eq!(
            mock.resource_version().as_deref(),
            Some("2"),
            "rv unchanged"
        );
    }

    /// GET on an empty store is a 404 that `get_opt` maps to `None`;
    /// DELETE clears the store.
    #[tokio::test]
    async fn mock_get_404_and_delete() {
        let (client, mock) = MockApiServer::new();
        let api: Api<Lease> = Api::default_namespaced(client);

        assert!(api.get_opt("l").await.expect("get_opt").is_none());

        api.create(&PostParams::default(), &lease("l", "n1", None))
            .await
            .expect("create");
        assert!(api.get_opt("l").await.expect("get_opt").is_some());

        assert!(mock.clear(), "clear reports a lease existed");
        assert!(api.get_opt("l").await.expect("get_opt").is_none());
    }
}
