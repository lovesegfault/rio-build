//! Env-propagation + `warn_on_spec_degrades` reachability tests.
//!
//! Two distinct clusters that share the mock-apiserver fixtures:
//!
//! - figment::Jail env-propagation tests — prove
//!   `build_statefulset` injects `LLVM_PROFILE_FILE`/`RUST_LOG`
//!   from the controller's own env (coverage-mode passthrough)
//! - P0365's `warn_on_spec_degrades` event-reason tests — prove the
//!   helper fires BEFORE the ephemeral early-return in `apply()`
//!
//! Split from the 1716L monolith (P0396).

use super::*;

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
            "/builderpools/test-pool/status",
            serde_json::json!({
                "apiVersion": "rio.build/v1alpha1",
                "kind": "BuilderPool",
                "metadata": {"name": "test-pool", "namespace": "rio"},
                "spec": {
                    "maxConcurrent": 10,
                    "autoscaling": {"metric": "x", "targetValue": 1},
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
        jail.set_env("RUST_LOG", "info,rio_builder=debug");

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
            Some("info,rio_builder=debug"),
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
    wp.spec.host_network = Some(true);
    wp.spec.privileged = None;

    let mut scenarios = vec![event_post_scenario(REASON_HOST_USERS_SUPPRESSED)];
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
    wp.spec.host_network = Some(true);
    wp.spec.privileged = None;

    // Event POST first, then the full apply() call chain.
    let mut scenarios = vec![event_post_scenario(REASON_HOST_USERS_SUPPRESSED)];
    scenarios.extend(ephemeral_reconcile_scenarios());
    let guard = verifier.run(scenarios);

    apply(Arc::new(wp), &ctx).await.expect("apply completes");

    guard.verified().await;
}
