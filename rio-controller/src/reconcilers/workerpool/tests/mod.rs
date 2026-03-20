//! WorkerPool reconciler test suite.
//!
//! Shared fixtures (`test_wp`, `test_sts`, `test_ctx`) are
//! `pub(super)` for the per-domain submodules extracted in
//! subsequent commits (P0396). Mirrors the prod seams in
//! `workerpool/{builders,disruption,ephemeral,mod}.rs`.

// r[verify ctrl.drain.all-then-scale]
// r[verify ctrl.drain.sigterm]

use std::collections::BTreeMap;

use super::builders::*;
use super::*;
use crate::crds::workerpool::SeccompProfileKind;
use crate::fixtures::{ApiServerVerifier, Scenario, apply_ok_scenarios, test_sched_addrs};

mod builders_tests;

/// Construct a minimal WorkerPool for builder tests. No K8s
/// interaction — pure struct-to-struct.
///
/// Delegates to the shared fixture. Local wrapper kept so the
/// 39 call sites across the split test modules don't need a
/// signature change.
pub(super) fn test_wp() -> WorkerPool {
    crate::fixtures::test_workerpool("test-pool")
}

/// Shorthand for tests: builds with default scheduler/store
/// addrs and replicas=Some(min). Use `build_statefulset`
/// directly for tests that care about those params.
pub(super) fn test_sts(wp: &WorkerPool) -> StatefulSet {
    build_statefulset(
        wp,
        wp.controller_owner_ref(&()).unwrap(),
        &test_sched_addrs(),
        "store:9002",
        Some(wp.spec.replicas.min),
    )
    .unwrap()
}

// =========================================================
// Mock-apiserver integration tests
//
// These test the WIRING: apply() calls Service PATCH then
// StatefulSet PATCH then WorkerPool/status PATCH, in that
// order, with server-side-apply params. The builder tests
// above cover WHAT gets patched; these cover WHEN/HOW.
// =========================================================

pub(super) fn test_ctx(client: kube::Client) -> Arc<Ctx> {
    let recorder = kube::runtime::events::Recorder::new(
        client.clone(),
        kube::runtime::events::Reporter {
            controller: "rio-controller-test".into(),
            instance: None,
        },
    );
    // Unreachable --- apply() doesn't touch the scheduler, and
    // cleanup() treats RPC failure as best-effort skip. connect_lazy
    // defers the TCP connect until the first RPC, which fails fast on
    // port 1 (never listened on).
    let dead_ch = tonic::transport::Endpoint::from_static("http://127.0.0.1:1").connect_lazy();
    Arc::new(Ctx {
        client,
        admin: rio_proto::AdminServiceClient::new(dead_ch),
        scheduler_addr: "http://127.0.0.1:1".into(),
        store_addr: "http://127.0.0.1:1".into(),
        scheduler_balance_host: None,
        scheduler_balance_port: 9001,
        recorder,
    })
}

/// Scenario matching a K8s Event POST from `warn_on_spec_degrades`.
/// `Recorder::publish` creates the event via POST to the events
/// .k8s.io/v1 API (first emission — not yet in the recorder's cache,
/// so it's a create not a merge-patch). `body_contains` asserts on
/// the `reason` field embedded in the event body.
///
/// Response body: minimal valid `events.k8s.io/v1/Event` shape. The
/// Recorder doesn't inspect the response content (it caches the
/// locally-constructed event, not the server's echo); we only need
/// kube's deserializer to accept it.
fn event_post_scenario(reason: &'static str) -> Scenario {
    Scenario {
        method: http::Method::POST,
        path_contains: "/apis/events.k8s.io/v1/namespaces/rio/events",
        body_contains: Some(reason),
        status: 200,
        body_json: serde_json::json!({
            "apiVersion": "events.k8s.io/v1",
            "kind": "Event",
            "metadata": {"name": "ev", "namespace": "rio"},
            "eventTime": null,
        })
        .to_string(),
    }
}

/// Scenarios for the ephemeral-mode path AFTER `warn_on_spec_degrades`:
/// `reconcile_ephemeral` lists Jobs → polls ClusterStatus (dead
/// admin → queued=0, no HTTP) → patches status. Only the two
/// apiserver calls appear in scenarios; the failed gRPC is just
/// a logged warn.
fn ephemeral_reconcile_scenarios() -> Vec<Scenario> {
    vec![
        // Jobs list — empty, so active=0 and to_spawn=0.
        Scenario::ok(
            http::Method::GET,
            "/apis/batch/v1/namespaces/rio/jobs",
            serde_json::json!({
                "apiVersion": "batch/v1",
                "kind": "JobList",
                "items": [],
            })
            .to_string(),
        ),
        // Status patch — reconcile_ephemeral reports active/ceiling.
        Scenario::ok(
            http::Method::PATCH,
            "/workerpools/test-pool/status",
            serde_json::json!({
                "apiVersion": "rio.build/v1alpha1",
                "kind": "WorkerPool",
                "metadata": {"name": "test-pool", "namespace": "rio"},
                "spec": {
                    "replicas": {"min": 0, "max": 10},
                    "autoscaling": {"metric": "x", "targetValue": 1},
                    "maxConcurrentBuilds": 1,
                    "fuseCacheSize": "1Gi",
                    "features": [],
                    "systems": ["x"],
                    "sizeClass": "x",
                    "image": "x",
                },
            })
            .to_string(),
        ),
    ]
}

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

// -------------------------------------------------------------------
// Coverage propagation (builders.rs LLVM_PROFILE_FILE check).
//
// figment::Jail serializes env access (global mutex) so parallel
// tests don't see each other's set_env/remove_var. Same pattern
// as rio-scheduler/src/lease.rs tests.
// -------------------------------------------------------------------

#[test]
#[allow(clippy::result_large_err)] // figment::Error is large, API-fixed
fn coverage_propagated_when_controller_env_set() -> figment::error::Result<()> {
    figment::Jail::expect_with(|jail| {
        jail.set_env("LLVM_PROFILE_FILE", "/var/lib/rio/cov/ctrl-%p-%m.profraw");

        let wp = test_wp();
        let sts = test_sts(&wp);
        let spec = sts.spec.unwrap().template.spec.unwrap();
        let container = &spec.containers[0];

        // Env var injected with the canonical pod-side template (NOT
        // the controller's own — the pod writes to the same mount
        // path, but %p/%m expand inside the pod).
        let envs = container.env.as_ref().expect("env vars");
        let profile_env = envs
            .iter()
            .find(|e| e.name == "LLVM_PROFILE_FILE")
            .expect("LLVM_PROFILE_FILE env injected");
        assert_eq!(
            profile_env.value.as_deref(),
            Some("/var/lib/rio/cov/rio-%p-%m.profraw")
        );

        // Volume: hostPath to /var/lib/rio/cov on the k8s node.
        let volumes = spec.volumes.as_ref().expect("volumes");
        let cov_vol = volumes
            .iter()
            .find(|v| v.name == "cov")
            .expect("cov volume");
        let hp = cov_vol.host_path.as_ref().expect("hostPath");
        assert_eq!(hp.path, "/var/lib/rio/cov");
        assert_eq!(hp.type_.as_deref(), Some("DirectoryOrCreate"));

        // Mount at the same path inside the container.
        let mounts = container.volume_mounts.as_ref().expect("mounts");
        let cov_mount = mounts.iter().find(|m| m.name == "cov").expect("cov mount");
        assert_eq!(cov_mount.mount_path, "/var/lib/rio/cov");

        Ok(())
    });
    Ok(())
}

#[test]
#[allow(clippy::result_large_err)]
fn rust_log_propagated_verbatim() -> figment::error::Result<()> {
    figment::Jail::expect_with(|jail| {
        jail.set_env("RUST_LOG", "info,rio_worker=debug");

        let wp = test_wp();
        let sts = test_sts(&wp);
        let container = &sts.spec.unwrap().template.spec.unwrap().containers[0];

        let envs = container.env.as_ref().expect("env vars");
        let rust_log = envs
            .iter()
            .find(|e| e.name == "RUST_LOG")
            .expect("RUST_LOG env injected");
        assert_eq!(
            rust_log.value.as_deref(),
            Some("info,rio_worker=debug"),
            "verbatim — per-crate EnvFilter directives preserved"
        );

        Ok(())
    });
    Ok(())
}

#[test]
#[allow(clippy::result_large_err)]
fn rust_log_absent_when_controller_env_unset() -> figment::error::Result<()> {
    figment::Jail::expect_with(|_jail| {
        // SAFETY: Jail's global mutex serializes env access.
        unsafe {
            std::env::remove_var("RUST_LOG");
        }

        let wp = test_wp();
        let sts = test_sts(&wp);
        let container = &sts.spec.unwrap().template.spec.unwrap().containers[0];

        let envs = container.env.as_ref().expect("env vars");
        assert!(
            !envs.iter().any(|e| e.name == "RUST_LOG"),
            "RUST_LOG should be absent — worker falls back to binary's info default"
        );

        Ok(())
    });
    Ok(())
}

#[test]
#[allow(clippy::result_large_err)]
fn coverage_absent_when_controller_env_unset() -> figment::error::Result<()> {
    figment::Jail::expect_with(|_jail| {
        // SAFETY: Jail serializes env access via a global mutex — no
        // other thread touches LLVM_PROFILE_FILE while we're here.
        // std::env::remove_var is unsafe in Rust 2024 for the general
        // "env is process-global" race reason; Jail's lock is the
        // synchronization that makes it safe here.
        unsafe {
            std::env::remove_var("LLVM_PROFILE_FILE");
        }

        let wp = test_wp();
        let sts = test_sts(&wp);
        let spec = sts.spec.unwrap().template.spec.unwrap();
        let container = &spec.containers[0];

        // NO env var, volume, or mount — prod pods are clean.
        let envs = container.env.as_ref().expect("env vars");
        assert!(
            !envs.iter().any(|e| e.name == "LLVM_PROFILE_FILE"),
            "coverage env should be absent in normal mode"
        );
        let volumes = spec.volumes.as_ref().expect("volumes");
        assert!(
            !volumes.iter().any(|v| v.name == "cov"),
            "cov volume should be absent"
        );
        let mounts = container.volume_mounts.as_ref().expect("mounts");
        assert!(
            !mounts.iter().any(|m| m.name == "cov"),
            "cov mount should be absent"
        );

        Ok(())
    });
    Ok(())
}

// =========================================================
// warn_on_spec_degrades reachability tests
//
// These prove the helper runs BEFORE the ephemeral early-return.
// The Recorder POSTs to the mock apiserver; the verifier asserts
// the POST arrives with the expected reason in the body. If the
// helper call moves AFTER the `if wp.spec.ephemeral { return ... }`
// branch, the event POST never happens for ephemeral pools and
// the verifier fails (scenario 0 = event POST; actual = jobs GET).
// =========================================================

/// Regression: ephemeral pool with hostNetwork:true gets the
/// HostUsersSuppressedForHostNetwork warning. Before the helper
/// extraction, the Warning-emit ran AFTER the ephemeral early-return
/// → unreachable for ephemeral. Now `warn_on_spec_degrades` runs
/// BEFORE the branch, both paths covered.
///
/// Mutation canary: move `warn_on_spec_degrades(&wp, ctx).await` to
/// AFTER the ephemeral early-return in `apply()` → this test fails
/// (verifier sees jobs-GET when expecting event-POST).
// r[verify ctrl.event.spec-degrade]
// r[verify ctrl.crd.host-users-network-exclusive]
#[tokio::test]
async fn warn_fires_for_ephemeral_with_host_network() {
    let (client, verifier) = ApiServerVerifier::new();
    let ctx = test_ctx(client);

    let mut wp = test_wp();
    wp.spec.ephemeral = true;
    wp.spec.replicas.min = 0; // CEL: ephemeral requires min==0
    wp.spec.host_network = Some(true);
    wp.spec.privileged = None;
    // Isolate the hostNetwork check — suppress the maxBuilds check
    // (test_wp() defaults to 4; ephemeral + >1 would fire the
    // second warning and add an extra POST scenario).
    wp.spec.max_concurrent_builds = 1;

    let mut scenarios = vec![event_post_scenario("HostUsersSuppressedForHostNetwork")];
    scenarios.extend(ephemeral_reconcile_scenarios());
    let guard = verifier.run(scenarios);

    apply(Arc::new(wp), &ctx)
        .await
        .expect("apply completes (reconcile_ephemeral path)");

    guard.verified().await;
}

/// Ephemeral pool with maxConcurrentBuilds>1 gets the
/// MaxConcurrentBuildsClampedForEphemeral warning. P0354 added the
/// env-replace clamp in `build_job`; this adds the operator-visible
/// half. The Recorder's POST body carries both the reason AND the
/// spec value (note contains "spec has 4").
// r[verify ctrl.pool.ephemeral-single-build]
#[tokio::test]
async fn warn_fires_for_ephemeral_with_maxbuilds_gt_1() {
    let (client, verifier) = ApiServerVerifier::new();
    let ctx = test_ctx(client);

    let mut wp = test_wp();
    wp.spec.ephemeral = true;
    wp.spec.replicas.min = 0;
    wp.spec.max_concurrent_builds = 4; // pre-CEL spec; CEL rejects NEW
    // host_network stays None → isolate the maxBuilds check.

    // body_contains="spec has 4" proves BOTH that the event fired
    // (POST reached the mock) AND that the note interpolates the
    // spec value (not a generic message). The reason string is
    // trivially present in the body too (it's the same POST), but
    // asserting on the note is the stronger check — it proves the
    // operator sees WHICH value is stale.
    let mut scenarios = vec![event_post_scenario("spec has 4")];
    scenarios.extend(ephemeral_reconcile_scenarios());
    let guard = verifier.run(scenarios);

    apply(Arc::new(wp), &ctx)
        .await
        .expect("apply completes (reconcile_ephemeral path)");

    guard.verified().await;
}

/// Non-regression: STS-mode (ephemeral=false) still emits the
/// hostNetwork warning after the helper extraction. T1 deleted the
/// inline event block that used to live at the old callsite; prove
/// the helper restores coverage for the STS-mode path too.
// r[verify ctrl.crd.host-users-network-exclusive]
#[tokio::test]
async fn warn_fires_for_sts_mode_with_host_network() {
    let (client, verifier) = ApiServerVerifier::new();
    let ctx = test_ctx(client);

    let mut wp = test_wp();
    wp.spec.ephemeral = false; // STS-mode (default, but explicit)
    wp.spec.host_network = Some(true);
    wp.spec.privileged = None;

    // Event POST first, then the full STS-mode apply() call chain.
    let mut scenarios = vec![event_post_scenario("HostUsersSuppressedForHostNetwork")];
    scenarios.extend(apply_ok_scenarios("test-pool", "rio", 2, false));
    let guard = verifier.run(scenarios);

    apply(Arc::new(wp), &ctx).await.expect("apply completes");

    guard.verified().await;
}
