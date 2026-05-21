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
//!   generated rather than hand-planned. A [`MockBehavior`] switch
//!   injects whole-apiserver failure modes (fail fast, hang) on top of
//!   the healthy state machine, for tests that drive a client loop
//!   through an outage-and-recovery schedule.
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

/// Failure-injection switch for [`MockApiServer`]: how the handler task
/// answers requests, independent of the stored Lease state.
///
/// The healthy dispatch is the method table on [`MockApiServer`]; the
/// failure modes model the two ways an apiserver outage looks from the
/// client side — an error response right away, or no response at all
/// (the client's own deadline is the only way out). Switching the
/// behavior never touches the stored lease, so a test can script
/// outage-and-recovery without re-seeding.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum MockBehavior {
    /// Serve requests against the stored Lease state (the method table
    /// in the [`MockApiServer`] docs).
    #[default]
    Healthy,
    /// Answer every request immediately with a 503 `ServiceUnavailable`
    /// `metav1.Status` — the "apiserver reachable but refusing" shape of
    /// an outage (kube clients surface it as `kube::Error::Api`).
    FailFast,
    /// Never answer: the response channel is parked (not dropped — a
    /// dropped channel surfaces as an immediate connection error, which
    /// is [`FailFast`](Self::FailFast) with extra steps). The client's
    /// request future pends until its own deadline fires — the
    /// "apiserver hung" shape of an outage.
    Hang,
}

/// The stored side of [`MockApiServer`]: at most one Lease object plus
/// the resourceVersion source the handler stamps writes from.
// r[impl ts.kube.lease-cas]
#[derive(Default)]
struct LeaseStore {
    /// The currently stored Lease object, if any.
    stored: Option<serde_json::Value>,
    /// Monotonic resourceVersion source mirroring the etcd global
    /// revision: the highest rv ever issued (or seeded) by this
    /// instance. Never reset, not even by DELETE/[`MockApiServer::clear`],
    /// so a recreated lease always takes a fresh rv and a snapshot of a
    /// previous incarnation can never pass the CAS again — exactly how a
    /// real apiserver behaves across delete/recreate.
    next_rv: u64,
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
/// | POST   | 409 if one exists, else store with the next resourceVersion from the monotonic counter. |
/// | PUT    | 409 unless the submitted `metadata.resourceVersion` equals the stored one (**the CAS**), else store with the rv bumped. |
/// | DELETE | clear the stored object; 200 if one existed, 404 otherwise. |
///
/// resourceVersions come from a per-instance monotonic counter that
/// survives DELETE and [`clear`](Self::clear) — like the etcd global
/// revision behind a real apiserver. A recreated lease therefore always
/// carries a strictly larger rv than every rv of the previous
/// incarnation, so a snapshot taken before the deletion can never pass
/// the PUT CAS against the recreated object (the model's `deleteLease`
/// encodes the same property).
///
/// The table above is the [`MockBehavior::Healthy`] dispatch (the
/// default). [`set_behavior`](Self::set_behavior) switches the whole
/// mock into a failure mode — every request answered 503, or no request
/// answered at all — for tests that script an apiserver outage around
/// the healthy state machine. The failure modes never touch the stored
/// lease, so flipping back to `Healthy` resumes from where the last
/// successful write left off.
///
/// The handler task runs until the [`Client`] is dropped. The returned
/// `MockApiServer` handle shares the stored state for inspection
/// (assertions on who won a race) and out-of-band manipulation (an
/// operator's `kubectl delete lease`, which is not something the code
/// under test performs).
pub struct MockApiServer {
    state: Arc<Mutex<LeaseStore>>,
    /// How the handler task answers requests; see [`MockBehavior`].
    /// Shared with the handler task so [`set_behavior`](Self::set_behavior)
    /// applies to every request pulled after the store.
    behavior: Arc<Mutex<MockBehavior>>,
}

impl MockApiServer {
    /// Create a mock Client backed by a fresh, empty Lease store and
    /// spawn the handler task (behavior [`MockBehavior::Healthy`]). The
    /// task exits when the Client (and every clone of it) is dropped.
    pub fn new() -> (Client, Self) {
        let (mock_service, handle) = mock::pair::<Request<Body>, Response<Body>>();
        let client = Client::new(mock_service, "default");
        let state = Arc::new(Mutex::new(LeaseStore::default()));
        let behavior = Arc::new(Mutex::new(MockBehavior::default()));
        let task_state = Arc::clone(&state);
        let task_behavior = Arc::clone(&behavior);
        tokio::spawn(async move {
            let mut handle = pin!(handle);
            // Hang mode parks the send halves here: never responded to,
            // never dropped (a drop completes the client's request
            // future with a connection error — a fast failure, not a
            // hang). The Vec lives as long as the handler task, i.e. as
            // long as the Client — exactly the window a hung request
            // must stay hung for.
            let mut parked = Vec::new();
            while let Some((request, send)) = handle.next_request().await {
                let behavior = *task_behavior.lock().expect("mock apiserver behavior lock");
                match behavior {
                    MockBehavior::Healthy => {
                        let method = request.method().clone();
                        let body_bytes = request
                            .into_body()
                            .collect()
                            .await
                            .expect("request body collectible")
                            .to_bytes();
                        send.send_response(Self::handle(&task_state, &method, &body_bytes));
                    }
                    MockBehavior::FailFast => send.send_response(Self::status(
                        503,
                        "ServiceUnavailable",
                        "injected outage: the mock apiserver is failing fast",
                    )),
                    MockBehavior::Hang => parked.push(send),
                }
            }
        });
        (client, Self { state, behavior })
    }

    /// Switch how the handler task answers subsequent requests. Takes
    /// effect for every request pulled after the store; requests already
    /// pulled keep the behavior they were dispatched under. The stored
    /// lease state is never touched, so a test can script
    /// `Healthy → FailFast → Hang → Healthy` sequences without
    /// re-seeding.
    pub fn set_behavior(&self, behavior: MockBehavior) {
        *self.behavior.lock().expect("mock apiserver behavior lock") = behavior;
    }

    /// Dispatch one request against the stored state. Synchronous so the
    /// mutex is never held across an await.
    fn handle(state: &Mutex<LeaseStore>, method: &http::Method, body: &[u8]) -> Response<Body> {
        let mut store = state.lock().expect("mock apiserver state lock");
        match *method {
            http::Method::GET => match store.stored.as_ref() {
                Some(obj) => Self::json(200, obj.to_string()),
                None => Self::status(404, "NotFound", "lease not found"),
            },
            http::Method::POST => {
                if store.stored.is_some() {
                    return Self::status(409, "AlreadyExists", "lease already exists");
                }
                let mut obj: serde_json::Value =
                    serde_json::from_slice(body).expect("POST body is a JSON Lease");
                store.next_rv += 1;
                obj["metadata"]["resourceVersion"] =
                    serde_json::Value::String(store.next_rv.to_string());
                let response = Self::json(201, obj.to_string());
                store.stored = Some(obj);
                response
            }
            http::Method::PUT => {
                let Some(current) = store.stored.as_ref() else {
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
                store.next_rv += 1;
                let mut obj = submitted;
                obj["metadata"]["resourceVersion"] =
                    serde_json::Value::String(store.next_rv.to_string());
                let response = Self::json(200, obj.to_string());
                store.stored = Some(obj);
                response
            }
            http::Method::DELETE => {
                // Drop the object only — next_rv keeps counting so the
                // next incarnation's rv stays above every rv this one
                // ever handed out.
                if store.stored.take().is_some() {
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
            .stored
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
    /// `metadata.resourceVersion`, which must be a numeric string — it
    /// raises the monotonic rv counter to at least that value, so
    /// subsequent writes bump from the seeded rv. Re-seeding a LOWER rv
    /// after prior activity leaves the counter at its high-water mark:
    /// later writes may skip numbers but never go backwards.
    pub fn seed(&self, lease: serde_json::Value) {
        let seeded_rv = lease["metadata"]["resourceVersion"]
            .as_str()
            .and_then(|rv| rv.parse::<u64>().ok())
            .expect("seeded metadata.resourceVersion must be present and numeric");
        let mut store = self.state.lock().expect("mock apiserver state lock");
        store.next_rv = store.next_rv.max(seeded_rv);
        store.stored = Some(lease);
    }

    /// Clear the stored lease out of band — an operator's
    /// `kubectl delete lease`, which the code under test never performs
    /// itself. The rv counter is NOT reset (it mirrors the etcd global
    /// revision), so a lease created after the clear takes a strictly
    /// larger resourceVersion than anything handed out before it.
    pub fn clear(&self) -> bool {
        self.state
            .lock()
            .expect("mock apiserver state lock")
            .stored
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
    use kube::api::{DeleteParams, ObjectMeta, PostParams};

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

    /// A snapshot taken before an HTTP DELETE can never pass the CAS
    /// against the recreated lease: the recreated incarnation takes a
    /// strictly larger resourceVersion (the rv source survives the
    /// deletion, mirroring the etcd global revision), so the stale PUT
    /// 409s instead of clobbering the new holder.
    // r[verify ts.kube.lease-cas]
    #[tokio::test]
    async fn mock_recreate_after_delete_rejects_pre_deletion_snapshot() {
        let (client, mock) = MockApiServer::new();
        let api: Api<Lease> = Api::default_namespaced(client);

        api.create(&PostParams::default(), &lease("l", "n1", None))
            .await
            .expect("first create succeeds");
        let stale_rv = api
            .get("l")
            .await
            .expect("get")
            .metadata
            .resource_version
            .expect("created lease carries a resourceVersion");

        api.delete("l", &DeleteParams::default())
            .await
            .expect("delete of an existing lease succeeds");
        assert!(mock.stored().is_none(), "delete empties the store");

        api.create(&PostParams::default(), &lease("l", "n2", None))
            .await
            .expect("recreate succeeds");

        let stale: u64 = stale_rv.parse().expect("stale resourceVersion is numeric");
        let recreated: u64 = mock
            .resource_version()
            .expect("recreated lease carries a resourceVersion")
            .parse()
            .expect("recreated resourceVersion is numeric");
        assert!(
            recreated > stale,
            "recreated lease must take a strictly larger resourceVersion \
             (stale {stale}, recreated {recreated})"
        );

        let err = api
            .replace(
                "l",
                &PostParams::default(),
                &lease("l", "n1", Some(&stale_rv)),
            )
            .await
            .expect_err("a pre-deletion snapshot rv must not pass the CAS");
        assert!(is_conflict(&err), "expected 409, got {err:?}");
        assert_eq!(
            mock.holder().as_deref(),
            Some("n2"),
            "the stale writer did not clobber the recreated lease"
        );
    }

    /// Same property through `clear()` — the out-of-band
    /// `kubectl delete lease` analogue: the resourceVersion source
    /// survives, so a pre-clear snapshot can never pass the CAS against
    /// the recreated lease.
    // r[verify ts.kube.lease-cas]
    #[tokio::test]
    async fn mock_recreate_after_clear_rejects_pre_deletion_snapshot() {
        let (client, mock) = MockApiServer::new();
        let api: Api<Lease> = Api::default_namespaced(client);

        api.create(&PostParams::default(), &lease("l", "n1", None))
            .await
            .expect("first create succeeds");
        let stale_rv = api
            .get("l")
            .await
            .expect("get")
            .metadata
            .resource_version
            .expect("created lease carries a resourceVersion");

        assert!(mock.clear(), "clear reports a lease existed");
        assert!(mock.stored().is_none(), "clear empties the store");

        api.create(&PostParams::default(), &lease("l", "n2", None))
            .await
            .expect("recreate succeeds");

        let stale: u64 = stale_rv.parse().expect("stale resourceVersion is numeric");
        let recreated: u64 = mock
            .resource_version()
            .expect("recreated lease carries a resourceVersion")
            .parse()
            .expect("recreated resourceVersion is numeric");
        assert!(
            recreated > stale,
            "recreated lease must take a strictly larger resourceVersion \
             (stale {stale}, recreated {recreated})"
        );

        let err = api
            .replace(
                "l",
                &PostParams::default(),
                &lease("l", "n1", Some(&stale_rv)),
            )
            .await
            .expect_err("a pre-clear snapshot rv must not pass the CAS");
        assert!(is_conflict(&err), "expected 409, got {err:?}");
        assert_eq!(
            mock.holder().as_deref(),
            Some("n2"),
            "the stale writer did not clobber the recreated lease"
        );
    }
}
