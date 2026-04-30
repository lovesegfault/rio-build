//! Env-propagation + `warn_on_spec_degrades` reachability tests.
//!
//! Two distinct clusters that share the mock-apiserver fixtures:
//!
//! - figment::Jail env-propagation tests — prove
//!   `build_pod_spec` injects `LLVM_PROFILE_FILE`/`RUST_LOG`
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
/// `reconcile` lists Jobs → polls ClusterStatus (dead
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
        // Pods list (report_terminated_pods) — empty, no
        // ReportExecutorTermination RPCs fired.
        Scenario::ok(
            http::Method::GET,
            "/api/v1/namespaces/rio/pods",
            serde_json::json!({
                "apiVersion": "v1",
                "kind": "PodList",
                "items": [],
            })
            .to_string(),
        ),
        // Status patch — reconcile reports active/ceiling.
        Scenario::ok(
            http::Method::PATCH,
            "/pools/test-pool/status",
            serde_json::json!({
                "apiVersion": "rio.build/v1alpha1",
                "kind": "Pool",
                "metadata": {"name": "test-pool", "namespace": "rio"},
                "spec": {
                    "kind": "Builder",
                    "maxConcurrent": 10,
                    "features": [],
                    "systems": ["x"],
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
        let spec = test_pod_spec(&wp);
        let container = &spec.containers[0];

        // Env var injected with the canonical pod-side template (NOT
        // the controller's own — the pod writes to the same mount
        // path, but %p/%m expand inside the pod). $(RIO_EXECUTOR_ID)
        // disambiguates per-pod: the cov hostPath is shared across all
        // executor pods on a node, each runs PID 1 → %p→1, same image
        // → %m identical.
        let envs = container.env.as_ref().expect("env vars");
        let profile_env = envs
            .iter()
            .find(|e| e.name == "LLVM_PROFILE_FILE")
            .expect("LLVM_PROFILE_FILE env injected");
        assert_eq!(
            profile_env.value.as_deref(),
            Some("/var/lib/rio/cov/rio-$(RIO_EXECUTOR_ID)-%p-%m.profraw")
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
        let spec = test_pod_spec(&wp);
        let container = &spec.containers[0];

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
        let spec = test_pod_spec(&wp);
        let container = &spec.containers[0];

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
        let spec = test_pod_spec(&wp);
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
// These prove the helper runs BEFORE the Job-spawn path. The
// Recorder POSTs to the mock apiserver; the verifier asserts the
// POST arrives with the expected reason in the body. If the helper
// call moves AFTER the spawn path, scenario 0 expects the event
// POST but sees a jobs GET instead.
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

    apply(Arc::new(wp), ctx.clone())
        .await
        .expect("apply completes (reconcile path)");

    guard.verified().await;
}

/// `r[ctrl.event.spec-degrade]` says "every spec field the builder
/// silently degrades" gets a Warning. Before the table-driven
/// `DEGRADE_CHECKS`, only `hostNetwork && !privileged` was covered —
/// the four Fetcher CEL rules and the non-CEL `wants_kvm` drop fired
/// silent overrides in `pod.rs` with no `kubectl get events` signal.
/// A pre-CEL `kind=Fetcher` Pool with `privileged:true` ran
/// unprivileged with the operator none the wiser.
// r[verify ctrl.event.spec-degrade]
#[tokio::test]
async fn warn_fires_for_every_degrade_check() {
    use crate::reconcilers::pool::DEGRADE_CHECKS;
    // Structural floor: 1 host-users + 5 Fetcher CEL rules + 1 Builder
    // fuseCacheBytes. New CEL rules without a DegradeCheck entry trip
    // this.
    assert!(
        DEGRADE_CHECKS.len() >= 7,
        "DEGRADE_CHECKS shrank below the count of silent overrides in \
         pod.rs::effective_* — every override needs a Warning entry"
    );

    // One fixture Pool that triggers every Fetcher check at once
    // (host-users covered by the test above). Verifier proves one
    // event POST per matching check fires before the reconcile body.
    let mut wp = crate::fixtures::test_pool("test-pool", ExecutorKind::Fetcher);
    wp.spec.privileged = Some(true);
    wp.spec.host_network = Some(true);
    wp.spec.seccomp_profile = Some(rio_crds::pool::SeccompProfileKind {
        type_: "Unconfined".into(),
        localhost_profile: None,
    });
    wp.spec.fuse_threads = Some(8);
    // NOT "kvm" — proves DEGRADE_CHECKS[5] fires for ANY non-empty
    // features (the I-181 ∅-guard starves on any value, not just kvm).
    wp.spec.features = vec!["big-parallel".into()];

    let (client, verifier) = ApiServerVerifier::new();
    let ctx = test_ctx(client);

    let mut scenarios: Vec<Scenario> = DEGRADE_CHECKS
        .iter()
        .filter(|c| (c.applies)(&wp.spec))
        .map(|c| event_post_scenario(c.reason))
        .collect();
    assert_eq!(
        scenarios.len(),
        5,
        "fixture should trigger every Fetcher degrade check"
    );
    scenarios.extend(ephemeral_reconcile_scenarios());
    let guard = verifier.run(scenarios);

    apply(Arc::new(wp), ctx.clone())
        .await
        .expect("apply completes (reconcile path)");

    guard.verified().await;
}

// r[verify ctrl.crd.host-users-network-exclusive]
/// `DEGRADE_CHECKS[0]` (`HostUsersSuppressed`) is Builder-only. The
/// pod.rs:327 suppression it mirrors only fires on the Builder path —
/// Fetchers always get `Some(false)` from `effective_host_users`
/// (never omitted) and `host_network=None` forced. A pre-CEL
/// `Fetcher{hostNetwork:true}` previously emitted a factually-wrong
/// "hostUsers omitted" + unactionable "Set privileged:true"; now only
/// the correct `FetcherHostNetworkSuppressed` (entry [2]) fires.
#[test]
fn degrade_host_users_suppressed_builder_only() {
    use crate::reconcilers::pool::{
        DEGRADE_CHECKS, REASON_FETCHER_HOST_NETWORK_SUPPRESSED, REASON_HOST_USERS_SUPPRESSED,
    };
    use rio_crds::pool::PoolSpec;

    let find = |reason: &str| {
        DEGRADE_CHECKS
            .iter()
            .find(|c| c.reason == reason)
            .expect("entry present")
    };
    let host_users = find(REASON_HOST_USERS_SUPPRESSED);
    let fetcher_hn = find(REASON_FETCHER_HOST_NETWORK_SUPPRESSED);

    // Fetcher: hostNetwork:true, privileged unset → entry [0] does
    // NOT fire (Builder-only); the correct Fetcher entry does.
    let fetcher = PoolSpec {
        kind: ExecutorKind::Fetcher,
        host_network: Some(true),
        privileged: None,
        ..crate::fixtures::test_pool("f", ExecutorKind::Fetcher).spec
    };
    assert!(
        !(host_users.applies)(&fetcher),
        "HostUsersSuppressed must NOT fire for Fetcher (factually wrong: \
         hostUsers is never omitted; advice unactionable)"
    );
    assert!(
        (fetcher_hn.applies)(&fetcher),
        "FetcherHostNetworkSuppressed is the correct event"
    );

    // Builder: same combo → entry [0] DOES fire.
    let builder = PoolSpec {
        kind: ExecutorKind::Builder,
        host_network: Some(true),
        privileged: None,
        ..crate::fixtures::test_pool("b", ExecutorKind::Builder).spec
    };
    assert!(
        (host_users.applies)(&builder),
        "HostUsersSuppressed fires for Builder (the pod.rs:327 suppression \
         it mirrors is on the Builder path)"
    );
}

/// mb_022: Builder pools setting `fuseCacheBytes` is the
/// `r[ctrl.event.spec-degrade]` shape (CEL at apply, Warning event for
/// pre-CEL CRs), NOT `Err(InvalidSpec)` halting the whole reconcile.
/// The field is ignored anyway (`pod::fuse_cache_bytes` reads
/// `BUILDER_FUSE_CACHE` for Builder kind) — the hard-reject was purely
/// punitive. This is the no-apiserver DEGRADE-path coverage; the CEL
/// layer is asserted in `nix/tests/helm/18-builder-fuse-cache-cel.sh`.
#[test]
fn degrade_builder_fuse_cache_ignored() {
    use crate::reconcilers::pool::{DEGRADE_CHECKS, REASON_BUILDER_FUSE_CACHE_IGNORED};
    use rio_crds::pool::PoolSpec;

    let check = DEGRADE_CHECKS
        .iter()
        .find(|c| c.reason == REASON_BUILDER_FUSE_CACHE_IGNORED)
        .expect("BuilderFuseCacheIgnored entry present");

    let builder = PoolSpec {
        fuse_cache_bytes: Some(100 * (1 << 30)),
        ..crate::fixtures::test_pool("b", ExecutorKind::Builder).spec
    };
    assert!(
        (check.applies)(&builder),
        "Builder with fuseCacheBytes set → Warning event (NOT hard-reject)"
    );

    let fetcher = PoolSpec {
        fuse_cache_bytes: Some(100 * (1 << 30)),
        ..crate::fixtures::test_pool("f", ExecutorKind::Fetcher).spec
    };
    assert!(
        !(check.applies)(&fetcher),
        "Fetcher may set fuseCacheBytes (per-pool override is supported)"
    );

    let builder_unset = crate::fixtures::test_pool("b2", ExecutorKind::Builder).spec;
    assert!(
        !(check.applies)(&builder_unset),
        "Builder without fuseCacheBytes → no Warning"
    );
}
