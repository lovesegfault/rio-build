use std::collections::BTreeMap;

use super::builders::*;
use super::*;
use crate::crds::workerpool::{Autoscaling, Replicas, WorkerPoolSpec};
use crate::fixtures::{ApiServerVerifier, Scenario, apply_ok_scenarios};

/// Construct a minimal WorkerPool for builder tests. No K8s
/// interaction — pure struct-to-struct.
fn test_wp() -> WorkerPool {
    let spec = WorkerPoolSpec {
        replicas: Replicas { min: 2, max: 10 },
        autoscaling: Autoscaling {
            metric: "queueDepth".into(),
            target_value: 5,
        },
        resources: None,
        max_concurrent_builds: 4,
        fuse_cache_size: "50Gi".into(),
        features: vec!["kvm".into()],
        systems: vec!["x86_64-linux".into()],
        size_class: "small".into(),
        image: "rio-worker:test".into(),
        image_pull_policy: None,
        node_selector: None,
        tolerations: None,
        privileged: None,
        host_network: None,
    };
    let mut wp = WorkerPool::new("test-pool", spec);
    // UID + namespace: controller_owner_ref needs these. In
    // real usage the apiserver sets them; tests fake them.
    wp.metadata.uid = Some("test-uid-123".into());
    wp.metadata.namespace = Some("rio".into());
    wp
}

/// Shorthand for tests: builds with default scheduler/store
/// addrs and replicas=Some(min). Use `build_statefulset`
/// directly for tests that care about those params.
fn test_sts(wp: &WorkerPool) -> StatefulSet {
    build_statefulset(
        wp,
        wp.controller_owner_ref(&()).unwrap(),
        "sched:9001",
        "store:9002",
        Some(wp.spec.replicas.min),
    )
    .unwrap()
}

#[test]
fn statefulset_has_owner_reference() {
    let wp = test_wp();
    let sts = test_sts(&wp);

    let orefs = sts.metadata.owner_references.expect("ownerRef set");
    assert_eq!(orefs.len(), 1);
    assert_eq!(orefs[0].kind, "WorkerPool");
    assert_eq!(orefs[0].name, "test-pool");
    assert_eq!(orefs[0].controller, Some(true), "controller=true for GC");
}

#[test]
fn statefulset_security_context() {
    let wp = test_wp();
    let sts = test_sts(&wp);

    let container = &sts.spec.unwrap().template.spec.unwrap().containers[0];
    let caps = container
        .security_context
        .as_ref()
        .unwrap()
        .capabilities
        .as_ref()
        .unwrap();
    let add = caps.add.as_ref().unwrap();
    assert!(add.contains(&"SYS_ADMIN".to_string()));
    assert!(add.contains(&"SYS_CHROOT".to_string()));
    // NOT privileged — capabilities are granular, privileged
    // disables seccomp. If someone adds privileged here, this
    // test fails.
    assert_eq!(
        container.security_context.as_ref().unwrap().privileged,
        None
    );
}

#[test]
fn statefulset_dev_fuse_volume() {
    let wp = test_wp();
    let sts = test_sts(&wp);

    let pod = sts.spec.unwrap().template.spec.unwrap();
    let fuse_vol = pod
        .volumes
        .unwrap()
        .into_iter()
        .find(|v| v.name == "dev-fuse")
        .expect("/dev/fuse volume");
    let hp = fuse_vol.host_path.expect("hostPath");
    assert_eq!(hp.path, "/dev/fuse");
    assert_eq!(
        hp.type_,
        Some("CharDevice".into()),
        "CharDevice type makes K8s verify it's a char device (catches no-FUSE nodes)"
    );
}

#[test]
fn statefulset_overlays_volume_mounted() {
    // RIO_OVERLAY_BASE_DIR points to /var/rio/overlays. If
    // there's no volume mount for it, it lands on the
    // container's root filesystem — which is overlayfs.
    // Overlayfs-as-upperdir can't create trusted.* xattrs →
    // every overlay mount fails with EINVAL. Regression guard.
    let wp = test_wp();
    let sts = test_sts(&wp);
    let pod = sts.spec.unwrap().template.spec.unwrap();

    let vol = pod
        .volumes
        .unwrap()
        .into_iter()
        .find(|v| v.name == "overlays")
        .expect("overlays volume must exist");
    assert!(
        vol.empty_dir.is_some(),
        "emptyDir → node's real disk (ext4/xfs with trusted.* xattr support)"
    );

    let mount = pod.containers[0]
        .volume_mounts
        .as_ref()
        .unwrap()
        .iter()
        .find(|m| m.name == "overlays")
        .expect("overlays volumeMount");
    assert_eq!(mount.mount_path, "/var/rio/overlays");
}

#[test]
fn statefulset_termination_grace() {
    let wp = test_wp();
    let sts = test_sts(&wp);
    let pod = sts.spec.unwrap().template.spec.unwrap();
    assert_eq!(
        pod.termination_grace_period_seconds,
        Some(7200),
        "2h for long nix builds (D3 drain sequence runs within this)"
    );
    assert_eq!(
        pod.automount_service_account_token,
        Some(false),
        "workers use gRPC, not K8s API — no SA token needed"
    );
}

#[test]
fn statefulset_env_vars() {
    let wp = test_wp();
    let sts = test_sts(&wp);

    let container = &sts.spec.unwrap().template.spec.unwrap().containers[0];
    let envs: BTreeMap<String, String> = container
        .env
        .as_ref()
        .unwrap()
        .iter()
        .filter_map(|e| e.value.clone().map(|v| (e.name.clone(), v)))
        .collect();

    assert_eq!(envs.get("RIO_SCHEDULER_ADDR"), Some(&"sched:9001".into()));
    assert_eq!(
        envs.get("RIO_STORE_ADDR"),
        Some(&"store:9002".into()),
        "from ctx.store_addr (NOT derived from scheduler — \
         different hostnames in kustomize base)"
    );
    assert_eq!(envs.get("RIO_MAX_BUILDS"), Some(&"4".into()));
    assert_eq!(envs.get("RIO_SIZE_CLASS"), Some(&"small".into()));
    assert_eq!(
        envs.get("RIO_FUSE_CACHE_SIZE_GB"),
        Some(&"50".into()),
        "50Gi parsed to 50 GB (binary → binary, integer)"
    );
    // systems + features: comma-sep strings. fixture wp has
    // systems=["x86_64-linux"], features=["kvm"]. Previously
    // these were SILENTLY DROPPED — CRD defined them, reconciler
    // never passed them, worker hardcoded features=Vec::new().
    assert_eq!(
        envs.get("RIO_SYSTEMS"),
        Some(&"x86_64-linux".into()),
        "systems comma-joined → worker's comma_vec deserialize"
    );
    assert_eq!(
        envs.get("RIO_FEATURES"),
        Some(&"kvm".into()),
        "features comma-joined (was silently dropped before)"
    );

    // RIO_WORKER_ID uses fieldRef, not value — check separately.
    let worker_id = container
        .env
        .as_ref()
        .unwrap()
        .iter()
        .find(|e| e.name == "RIO_WORKER_ID")
        .unwrap();
    assert_eq!(worker_id.value, None, "not a literal value");
    assert_eq!(
        worker_id
            .value_from
            .as_ref()
            .unwrap()
            .field_ref
            .as_ref()
            .unwrap()
            .field_path,
        "metadata.name",
        "downward API: pod name (StatefulSet ordinal, unique)"
    );
}

#[test]
fn statefulset_replicas_starts_at_min() {
    let wp = test_wp();
    let sts = test_sts(&wp);
    assert_eq!(
        sts.spec.unwrap().replicas,
        Some(2),
        "initial create: set to spec.replicas.min"
    );
}

/// replicas=None → field omitted from the SSA patch → the
/// autoscaler's field-manager ownership is preserved.
/// Without this, every reconcile would revert the autoscaler's
/// scaling decision to min (SSA .force() takes ownership of
/// every field in the patch).
///
/// k8s-openapi's custom Serialize impl skips Option::None
/// fields, so None → field absent → SSA leaves it alone.
#[test]
fn statefulset_replicas_omitted_when_none() {
    let wp = test_wp();
    let sts = build_statefulset(
        &wp,
        wp.controller_owner_ref(&()).unwrap(),
        "sched:9001",
        "store:9002",
        None,
    )
    .unwrap();

    assert_eq!(
        sts.spec.as_ref().unwrap().replicas,
        None,
        "subsequent reconciles: replicas=None → autoscaler owns it"
    );

    // And the crucial part: serialized JSON doesn't contain
    // the field. SSA semantics: absent field = "I don't
    // manage this." Verifies k8s-openapi's skip-None-on-
    // serialize behavior hasn't regressed.
    let json = serde_json::to_string(&sts).unwrap();
    assert!(
        !json.contains("\"replicas\""),
        "replicas must be absent from serialized JSON for SSA to \
         preserve autoscaler ownership. Found in: {json}"
    );
}

#[test]
fn statefulset_image_pull_policy_passthrough() {
    // None stays None — K8s applies its tag-based default
    // (IfNotPresent for non-:latest, Always for :latest).
    let wp = test_wp();
    let sts = test_sts(&wp);
    let container = &sts.spec.unwrap().template.spec.unwrap().containers[0];
    assert_eq!(container.image_pull_policy, None);

    // Explicit value passes through. Airgap k3s/kind need
    // IfNotPresent or Never to use ctr-imported images.
    let mut wp = test_wp();
    wp.spec.image_pull_policy = Some("IfNotPresent".into());
    let sts = test_sts(&wp);
    let container = &sts.spec.unwrap().template.spec.unwrap().containers[0];
    assert_eq!(container.image_pull_policy.as_deref(), Some("IfNotPresent"));
}

// ----- quantity parsing -----

#[test]
fn quantity_binary_suffix() {
    assert_eq!(parse_quantity_to_gb("50Gi").unwrap(), 50);
    assert_eq!(parse_quantity_to_gb("100Gi").unwrap(), 100);
    assert_eq!(
        parse_quantity_to_gb("1Ti").unwrap(),
        1024,
        "1 TiB = 1024 GiB"
    );
    // Mi rounds DOWN to GB. 2048 MiB = 2 GiB exactly.
    assert_eq!(parse_quantity_to_gb("2048Mi").unwrap(), 2);
    // 1500 MiB = 1.46 GiB → rounds down to 1.
    assert_eq!(
        parse_quantity_to_gb("1500Mi").unwrap(),
        1,
        "rounds DOWN (cache limit: better under than over → kubelet eviction)"
    );
}

#[test]
fn quantity_decimal_suffix() {
    // 100G = 100 * 10^9 bytes = 93.13 GiB → 93.
    assert_eq!(parse_quantity_to_gb("100G").unwrap(), 93);
    // 50G = 46.56 GiB → 46.
    assert_eq!(parse_quantity_to_gb("50G").unwrap(), 46);
}

#[test]
fn quantity_raw_bytes() {
    // 107374182400 = 100 * 1024^3 = 100 GiB exactly.
    assert_eq!(parse_quantity_to_gb("107374182400").unwrap(), 100);
}

#[test]
fn quantity_suffix_order_gi_before_g() {
    // If we matched "G" before "Gi", "100Gi" would strip "G"
    // leaving "100i" which doesn't parse. The SUFFIXES const
    // is ordered to prevent this. If someone reorders it,
    // this test catches it.
    assert_eq!(
        parse_quantity_to_gb("100Gi").unwrap(),
        100,
        "Gi matched, not G"
    );
}

#[test]
fn quantity_invalid_rejected() {
    assert!(matches!(
        parse_quantity_to_gb("not-a-number"),
        Err(Error::InvalidSpec(_))
    ));
    assert!(matches!(
        parse_quantity_to_gb("100Pi"),
        Err(Error::InvalidSpec(_))
    ));
    // Empty
    assert!(matches!(
        parse_quantity_to_gb(""),
        Err(Error::InvalidSpec(_))
    ));
}

#[test]
fn quantity_invalidspec_from_statefulset() {
    let mut wp = test_wp();
    wp.spec.fuse_cache_size = "garbage".into();
    let result = build_statefulset(
        &wp,
        wp.controller_owner_ref(&()).unwrap(),
        "sched:9001",
        "store:9002",
        Some(1),
    );
    match result {
        Err(Error::InvalidSpec(msg)) => {
            assert!(msg.contains("fuseCacheSize"));
            assert!(msg.contains("garbage"));
        }
        other => panic!("expected InvalidSpec with helpful message, got {other:?}"),
    }
}

// =========================================================
// Mock-apiserver integration tests
//
// These test the WIRING: apply() calls Service PATCH then
// StatefulSet PATCH then WorkerPool/status PATCH, in that
// order, with server-side-apply params. The builder tests
// above cover WHAT gets patched; these cover WHEN/HOW.
// =========================================================

fn test_ctx(client: kube::Client) -> Arc<Ctx> {
    Arc::new(Ctx {
        client,
        // Unreachable — apply() doesn't touch the scheduler,
        // and cleanup() treats connect failure as best-effort
        // skip. Using an address that fails fast (port 1 is
        // never listened on) vs one that times out.
        scheduler_addr: "http://127.0.0.1:1".into(),
        store_addr: "http://127.0.0.1:1".into(),
    })
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
    let task = verifier.run(apply_ok_scenarios("test-pool", "rio", 2, false));

    // Call apply() directly (not reconcile() — finalizer()
    // would do its own GET + PATCH of metadata.finalizers
    // first, which adds scenarios we don't care about here).
    let action = apply(wp, &ctx).await.expect("apply succeeds");

    // Requeue in 5m — the fallback re-reconcile.
    assert_eq!(action, Action::requeue(Duration::from_secs(300)));

    tokio::time::timeout(Duration::from_secs(1), task)
        .await
        .expect("verifier consumed all scenarios (right number of calls)")
        .expect("verifier assertions passed");
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
    let task = verifier.run(vec![
        Scenario::ok(
            http::Method::PATCH,
            "fieldManager=rio-controller",
            serde_json::json!({"metadata":{"name":"test-pool-workers"}}).to_string(),
        ),
        // GET before STS PATCH (replicas-ownership check).
        // 404 → first-create → replicas set to min.
        Scenario {
            method: http::Method::GET,
            path_contains: "/statefulsets/test-pool-workers",
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
    tokio::time::timeout(Duration::from_secs(1), task)
        .await
        .unwrap()
        .unwrap();
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
    let task = verifier.run(vec![
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

    tokio::time::timeout(Duration::from_secs(1), task)
        .await
        .unwrap()
        .unwrap();
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

    let task = verifier.run(vec![
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

    tokio::time::timeout(Duration::from_secs(1), task)
        .await
        .unwrap()
        .unwrap();
}
