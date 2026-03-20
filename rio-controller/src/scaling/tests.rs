//! Unit tests for the shared scaling surface: SSA body builders,
//! `compute_desired`, stabilization windows, and the `is_wps_owned*`
//! / `find_wps_child` ownership predicates.
//!
//! Lives in its own file (same P0356 pattern as `grpc/tests.rs`)
//! so `scaling/mod.rs` stays under the collision-surface target.

// r[verify ctrl.autoscale.direct-patch]
// r[verify ctrl.autoscale.separate-field-manager]

use std::time::{Duration, Instant};

use kube::ResourceExt;

use super::*;
use crate::crds::workerpool::WorkerPool;
use crate::crds::workerpoolset::WorkerPoolSet;

// ---- sts_replicas_patch: SSA body shape ----

/// SSA patches MUST carry apiVersion + kind, or the apiserver
/// returns 400 "apiVersion must be set". A body like
/// `{"spec":{"replicas":N}}` silently fails every scale. This
/// test is a tripwire: if someone strips the GVK fields "for
/// brevity," this breaks.
/// WorkerPool status patch needs apiVersion+kind (same SSA
/// requirement as the STS replicas patch). Also verify it's a
/// PARTIAL status: replicas/ready/desired absent (reconciler's
/// fields), lastScaleTime + conditions present (ours).
#[test]
fn wp_status_patch_has_gvk_and_partial_status() {
    let cond = scaling_condition("True", "ScaledUp", "from 1 to 3");
    let patch = wp_status_patch(std::slice::from_ref(&cond));

    // GVK: SSA rejects without these.
    assert!(
        patch.get("apiVersion").and_then(|v| v.as_str()).is_some(),
        "SSA body without apiVersion → 400"
    );
    assert_eq!(
        patch.get("kind").and_then(|v| v.as_str()),
        Some("WorkerPool")
    );

    let status = patch.get("status").expect("status key");
    // Autoscaler's fields present.
    assert!(
        status.get("lastScaleTime").is_some(),
        "autoscaler owns this"
    );
    assert_eq!(
        status
            .get("conditions")
            .and_then(|c| c.as_array())
            .map(|a| a.len()),
        Some(1),
        "one Scaling condition"
    );
    // Reconciler's fields ABSENT — SSA field ownership split.
    // If we included these, our patch would fight the
    // reconciler's on every scale.
    assert!(
        status.get("replicas").is_none(),
        "reconciler owns replicas; our patch must not touch it"
    );
    assert!(status.get("desiredReplicas").is_none());
}

/// Scaling condition has the standard K8s Condition shape
/// (type/status/reason/message/lastTransitionTime). kubectl
/// describe reads these fields by convention.
#[test]
fn scaling_condition_has_standard_fields() {
    let c = scaling_condition("False", "UnknownMetric", "metric 'foo' unsupported");
    assert_eq!(c.get("type").and_then(|v| v.as_str()), Some("Scaling"));
    assert_eq!(c.get("status").and_then(|v| v.as_str()), Some("False"));
    assert_eq!(
        c.get("reason").and_then(|v| v.as_str()),
        Some("UnknownMetric")
    );
    assert!(c.get("message").is_some());
    // lastTransitionTime is an RFC3339 timestamp. Cheap check:
    // contains T and ends with Z (UTC). Full parse in k8s
    // apiserver; this just catches the obvious "we passed
    // unix-epoch-secs as a number" class of mistakes.
    let ts = c
        .get("lastTransitionTime")
        .and_then(|v| v.as_str())
        .expect("timestamp string");
    assert!(
        ts.contains('T') && ts.ends_with('Z'),
        "RFC3339 UTC format; got {ts}"
    );
}

#[test]
fn sts_replicas_patch_has_gvk() {
    let patch = sts_replicas_patch(5);
    assert_eq!(
        patch.get("apiVersion").and_then(|v| v.as_str()),
        Some("apps/v1"),
        "SSA body without apiVersion → apiserver 400"
    );
    assert_eq!(
        patch.get("kind").and_then(|v| v.as_str()),
        Some("StatefulSet"),
        "SSA body without kind → apiserver 400"
    );
    assert_eq!(
        patch
            .get("spec")
            .and_then(|s| s.get("replicas"))
            .and_then(|r| r.as_i64()),
        Some(5),
        "the actual payload"
    );
}

// ---- compute_desired: pure arithmetic ----

#[test]
fn compute_desired_basic() {
    // 15 queued, target 5 per worker → need 3 workers.
    assert_eq!(compute_desired(15, 5, 1, 10), 3);
    // 16 queued, target 5 → ceil(16/5) = 4.
    assert_eq!(compute_desired(16, 5, 1, 10), 4);
}

#[test]
fn compute_desired_clamps() {
    // 100 queued, target 5 → 20, but max=10.
    assert_eq!(compute_desired(100, 5, 1, 10), 10);
    // 0 queued → 0, but min=2.
    assert_eq!(compute_desired(0, 5, 2, 10), 2, "empty queue → min, not 0");
}

#[test]
fn compute_desired_target_zero_clamped() {
    // target=0 would div-by-zero. Clamped to 1. 5 queued → 5.
    assert_eq!(compute_desired(5, 0, 1, 10), 5);
    // Negative target (shouldn't happen via CRD, but be safe).
    assert_eq!(compute_desired(5, -3, 1, 10), 5);
}

#[test]
fn compute_desired_no_wrap_at_high_queue() {
    // queued > i32::MAX — pathological, but in u32 range.
    // A naive `raw as i32` wraps negative → .clamp(min,max)
    // returns min → autoscaler scales DOWN under extreme load.
    // Bounding to i32::MAX first makes it clamp to max.
    let queued = u32::MAX; // > 4 billion
    let got = compute_desired(queued, 1, 2, 100);
    assert_eq!(
        got, 100,
        "high queue must clamp to max, not wrap negative → min"
    );
}

/// min > max would PANIC in i32::clamp. Defensive swap prevents
/// the autoscaler loop from dying silently on a pre-CEL CRD (or
/// a --validate=false edit).
#[test]
fn compute_desired_swaps_min_greater_than_max() {
    // min=10, max=2 → swap to min=2, max=10. With queued=50,
    // target=5 → desired=10 (at max).
    assert_eq!(
        compute_desired(50, 5, 10, 2),
        10,
        "min>max swapped; clamped to original min (now max)"
    );
    // queued=0 → desired=0 → clamped to swapped min (original max=2).
    assert_eq!(
        compute_desired(0, 5, 10, 2),
        2,
        "queued=0 → swapped min (original max)"
    );
}

// ---- check_stabilization: timing logic ----
//
// We manually construct Instants (subtract durations from
// now()) to avoid real sleeps. Tests use the DEFAULT timing
// (30s up-window, 30s min-interval); production may use
// different values via config but the LOGIC is the same.

const T: ScalingTiming = ScalingTiming {
    poll_interval: Duration::from_secs(30),
    scale_up_window: Duration::from_secs(30),
    scale_down_window: Duration::from_secs(600),
    min_scale_interval: Duration::from_secs(30),
};

fn mk_state(initial: i32) -> ScaleState {
    ScaleState::new(initial, T.min_scale_interval)
}

/// Fresh state with a desired that differs from current →
/// first call returns DesiredChanged (new desired seen, window
/// reset). Second call with same desired and enough time
/// elapsed → Patch.
#[test]
fn stabilization_window_before_patch() {
    let mut state = mk_state(2);
    // Manually age stable_since past the window. We do this
    // instead of sleeping because real sleeps in tests are
    // flaky (CI noisy neighbors).
    state.stable_since = Instant::now()
        .checked_sub(T.scale_up_window + Duration::from_secs(1))
        .unwrap();

    // First call: desired=5 is new (state has last_desired=2
    // from init). Reset.
    let d1 = check_stabilization(&mut state, 2, 5, T);
    assert!(
        matches!(d1, Decision::Wait(WaitReason::DesiredChanged)),
        "new desired → reset window"
    );
    // stable_since was just reset to now.

    // Second call, same desired, but window hasn't elapsed.
    let d2 = check_stabilization(&mut state, 2, 5, T);
    assert!(
        matches!(d2, Decision::Wait(WaitReason::Stabilizing)),
        "window not elapsed"
    );

    // Age it.
    state.stable_since = Instant::now()
        .checked_sub(T.scale_up_window + Duration::from_secs(1))
        .unwrap();
    let d3 = check_stabilization(&mut state, 2, 5, T);
    assert!(
        matches!(d3, Decision::Patch(Direction::Up)),
        "window elapsed, same desired → patch"
    );
}

#[test]
fn stabilization_down_window_longer() {
    let mut state = mk_state(10);
    state.last_desired = 3; // as if we've seen 3 before

    // Age past T.scale_up_window but NOT T.scale_down_window.
    state.stable_since = Instant::now()
        .checked_sub(T.scale_up_window + Duration::from_secs(60))
        .unwrap();

    // desired=3 < current=10 → scale DOWN. up-window elapsed
    // but down-window (10 min) hasn't.
    let d = check_stabilization(&mut state, 10, 3, T);
    assert!(
        matches!(d, Decision::Wait(WaitReason::Stabilizing)),
        "scale-down needs the 10min window, not the 30s one"
    );

    // Age past down window.
    state.stable_since = Instant::now()
        .checked_sub(T.scale_down_window + Duration::from_secs(1))
        .unwrap();
    let d2 = check_stabilization(&mut state, 10, 3, T);
    assert!(matches!(d2, Decision::Patch(Direction::Down)));
}

#[test]
fn stabilization_no_change() {
    let mut state = mk_state(5);
    state.last_desired = 5;
    let d = check_stabilization(&mut state, 5, 5, T);
    assert!(matches!(d, Decision::Wait(WaitReason::NoChange)));
}

#[test]
fn stabilization_anti_flap() {
    let mut state = mk_state(2);
    state.last_desired = 5;
    // Window elapsed.
    state.stable_since = Instant::now()
        .checked_sub(T.scale_up_window + Duration::from_secs(1))
        .unwrap();
    // But last_patch was just now.
    state.last_patch = Instant::now();

    let d = check_stabilization(&mut state, 2, 5, T);
    assert!(
        matches!(d, Decision::Wait(WaitReason::AntiFlap)),
        "too soon since last patch, even though window elapsed"
    );
}

#[test]
fn stabilization_desired_wobble_resets() {
    // desired changes 5 → 6 → 5. Each change resets. The
    // 5 → 6 → 5 round trip doesn't get to "5 was stable for
    // 30s" because the window reset when it hit 6.
    let mut state = mk_state(2);
    state.last_desired = 5;
    state.stable_since = Instant::now()
        .checked_sub(T.scale_up_window + Duration::from_secs(1))
        .unwrap();

    // Wobble to 6.
    let d = check_stabilization(&mut state, 2, 6, T);
    assert!(matches!(d, Decision::Wait(WaitReason::DesiredChanged)));
    assert_eq!(state.last_desired, 6);

    // Wobble back to 5. Resets AGAIN.
    let d = check_stabilization(&mut state, 2, 5, T);
    assert!(
        matches!(d, Decision::Wait(WaitReason::DesiredChanged)),
        "5 → 6 → 5 resets twice; 'stable' means no changes in window"
    );
}

// ---- WPS per-class autoscaler: SSA field manager + ownership ----

// r[verify ctrl.wps.autoscale]
/// `is_wps_owned` must return true for pools with a
/// `controller=true` WorkerPoolSet owner reference, false
/// otherwise. The distinction is LOAD-BEARING: a false
/// positive makes standalone pools silently un-scaled; a
/// false negative makes WPS children double-scaled (the
/// standalone loop's cluster-wide depth fights the per-class
/// loop's class-specific depth → flap).
#[test]
fn is_wps_owned_detects_controller_ownerref() {
    use k8s_openapi::apimachinery::pkg::apis::meta::v1::OwnerReference;

    let spec = crate::fixtures::test_workerpool_spec();

    // Standalone pool: no owner reference at all.
    let standalone = WorkerPool::new("standalone-pool", spec.clone());
    assert!(
        !is_wps_owned(&standalone),
        "no ownerRef → NOT WPS-owned (standalone pools must be scaled by the cluster-wide loop)"
    );

    // WPS child: ownerRef kind=WorkerPoolSet, controller=true.
    // Mirrors what builders::build_child_workerpool sets via
    // controller_owner_ref(&()).
    let mut wps_child = WorkerPool::new("test-wps-small", spec.clone());
    wps_child.metadata.owner_references = Some(vec![OwnerReference {
        api_version: "rio.build/v1alpha1".into(),
        kind: "WorkerPoolSet".into(),
        name: "test-wps".into(),
        uid: "wps-uid-456".into(),
        controller: Some(true),
        block_owner_deletion: Some(true),
    }]);
    assert!(
        is_wps_owned(&wps_child),
        "WorkerPoolSet ownerRef with controller=true → WPS-owned"
    );

    // Owned by something ELSE (e.g., a Helm chart adopting a
    // WorkerPool). controller=true but wrong kind.
    let mut other_owned = WorkerPool::new("other-owned", spec.clone());
    other_owned.metadata.owner_references = Some(vec![OwnerReference {
        api_version: "apps/v1".into(),
        kind: "Deployment".into(),
        name: "helm-release".into(),
        uid: "helm-uid".into(),
        controller: Some(true),
        block_owner_deletion: None,
    }]);
    assert!(
        !is_wps_owned(&other_owned),
        "non-WorkerPoolSet ownerRef → NOT WPS-owned (kind check is load-bearing)"
    );

    // WPS ownerRef but controller=None (garbage-collection
    // owner, not controller). The reconciler always sets
    // controller=true; this case shouldn't happen but
    // defensive: controller=None means NOT controller-owned.
    let mut gc_only = WorkerPool::new("gc-only", spec);
    gc_only.metadata.owner_references = Some(vec![OwnerReference {
        api_version: "rio.build/v1alpha1".into(),
        kind: "WorkerPoolSet".into(),
        name: "test-wps".into(),
        uid: "wps-uid-789".into(),
        controller: None,
        block_owner_deletion: None,
    }]);
    assert!(
        !is_wps_owned(&gc_only),
        "controller=None → NOT WPS-owned (GC-only owner doesn't skip cluster-wide scaling)"
    );
}

// ---- find_wps_child: name-match + ownerRef gate ----
//
// Test helpers for the find_wps_child cases. These mirror the
// builders::tests::test_wps_with_classes pattern but live here
// because that helper is in a private #[cfg(test)] mod inside
// builders.rs (unreachable cross-module). WorkerPoolSpec shape
// is pulled from crate::fixtures::test_workerpool_spec — the
// single touch point for E0063 on field adds.
//
// pub(super) so per_class.rs's tests can reuse them.

pub(super) fn test_wps(name: &str, ns: &str, class_names: &[&str]) -> WorkerPoolSet {
    use crate::crds::workerpoolset::{PoolTemplate, SizeClassSpec, WorkerPoolSetSpec};
    use k8s_openapi::api::core::v1::ResourceRequirements;
    let classes = class_names
        .iter()
        .map(|n| SizeClassSpec {
            name: (*n).to_string(),
            cutoff_secs: 60.0,
            min_replicas: Some(1),
            max_replicas: Some(10),
            target_queue_per_replica: Some(5),
            resources: ResourceRequirements::default(),
        })
        .collect();
    let spec = WorkerPoolSetSpec {
        classes,
        pool_template: PoolTemplate {
            image: "rio-worker:test".into(),
            systems: vec!["x86_64-linux".into()],
            ..Default::default()
        },
        cutoff_learning: None,
    };
    let mut wps = WorkerPoolSet::new(name, spec);
    wps.metadata.uid = Some(format!("{name}-uid"));
    wps.metadata.namespace = Some(ns.into());
    wps
}

pub(super) fn test_wp_in_ns(name: &str, ns: &str) -> WorkerPool {
    let mut wp = WorkerPool::new(name, crate::fixtures::test_workerpool_spec());
    wp.metadata.namespace = Some(ns.into());
    wp
}

pub(super) fn with_wps_owner(mut wp: WorkerPool, wps_name: &str, wps_uid: &str) -> WorkerPool {
    use k8s_openapi::apimachinery::pkg::apis::meta::v1::OwnerReference;
    wp.metadata.owner_references = Some(vec![OwnerReference {
        api_version: "rio.build/v1alpha1".into(),
        kind: "WorkerPoolSet".into(),
        name: wps_name.into(),
        uid: wps_uid.into(),
        controller: Some(true),
        block_owner_deletion: Some(true),
    }]);
    wp
}

/// Positive control for `find_wps_child`: a properly-owned
/// child (name matches AND has WPS controller ownerRef) is
/// returned as `Found`. This plus the NameCollision test above
/// prove the two-key symmetry: name-match alone is insufficient,
/// ownerRef alone is checked by `is_wps_owned` (standalone-loop
/// skip), and BOTH together gate per-class scaling.
#[test]
fn find_wps_child_returns_found_for_owned_child() {
    let wps = test_wps("prod", "rio", &["small", "large"]);
    let small = with_wps_owner(test_wp_in_ns("prod-small", "rio"), "prod", "prod-uid");
    let large = with_wps_owner(test_wp_in_ns("prod-large", "rio"), "prod", "prod-uid");
    // A standalone pool in the same namespace — must not
    // interfere with the lookup (name doesn't match).
    let standalone = test_wp_in_ns("unrelated-pool", "rio");
    let pools = [small, large, standalone];

    // "small" → Found(prod-small).
    match find_wps_child(&wps, "small", &pools) {
        ChildLookup::Found(c) => assert_eq!(c.name_any(), "prod-small"),
        other => panic!("expected Found for owned child, got {other:?}"),
    }

    // Nonexistent class → NotCreated.
    match find_wps_child(&wps, "xlarge", &pools) {
        ChildLookup::NotCreated => {}
        other => panic!("expected NotCreated for missing child, got {other:?}"),
    }
}

/// Namespace scoping: a pool matching the name but in a
/// DIFFERENT namespace is `NotCreated`, not `NameCollision`
/// or `Found`. Pool names are namespace-scoped; a `prod-small`
/// in `rio-dev` is not the `prod` WPS's child in `rio-prod`.
#[test]
fn find_wps_child_respects_namespace() {
    let wps = test_wps("prod", "rio-prod", &["small"]);
    // Same name, WRONG namespace. Even with a matching ownerRef
    // this shouldn't be found — namespaces are boundaries.
    let wrong_ns = with_wps_owner(test_wp_in_ns("prod-small", "rio-dev"), "prod", "prod-uid");

    match find_wps_child(&wps, "small", std::slice::from_ref(&wrong_ns)) {
        ChildLookup::NotCreated => {} // expected — no name+ns match
        other => panic!(
            "cross-namespace pool should be NotCreated (namespaces are boundaries), got {other:?}"
        ),
    }
}

/// `is_wps_owned_by` checks ownerRef UID, not just kind. The
/// prune path uses this — matching by kind alone (like
/// `is_wps_owned`) would prune a DIFFERENT WPS's children if
/// two WPS objects share a namespace. UID is the only stable
/// identifier across renames/recreates.
///
/// The load-bearing case: two WPS in the same namespace, one
/// removes a class. `prune_stale_children` must delete ONLY
/// that WPS's orphan — not the other WPS's still-active child.
// r[verify ctrl.wps.prune-stale]
#[test]
fn is_wps_owned_by_matches_uid_not_just_kind() {
    let wps_a = test_wps("wps-a", "rio", &["small"]);
    let wps_b = test_wps("wps-b", "rio", &["small"]);

    // Child owned by wps-a (UID = "wps-a-uid" per test_wps).
    let child_a = with_wps_owner(test_wp_in_ns("wps-a-small", "rio"), "wps-a", "wps-a-uid");

    // Owned by wps-a — is_wps_owned_by(_, wps_a) = true.
    assert!(
        is_wps_owned_by(&child_a, &wps_a),
        "child with matching ownerRef UID → owned by that WPS"
    );
    // NOT owned by wps-b. Same kind, same namespace, WRONG UID.
    // This is the discriminator — prune must not touch wps-b's
    // children when wps-a removes a class.
    assert!(
        !is_wps_owned_by(&child_a, &wps_b),
        "child with non-matching UID → NOT owned (prune must not cross WPS boundaries)"
    );

    // is_wps_owned (kind-only) says true for both — proving
    // the UID check is what distinguishes is_wps_owned_by.
    assert!(
        is_wps_owned(&child_a),
        "kind-only check is insufficient (both WPS would match)"
    );

    // No ownerRef at all → false (defensive: prune must not
    // touch standalone pools).
    let standalone = test_wp_in_ns("standalone", "rio");
    assert!(!is_wps_owned_by(&standalone, &wps_a));

    // WPS with no UID (not from apiserver, shouldn't happen in
    // real reconcile) → false (can't prove ownership).
    let mut wps_no_uid = test_wps("no-uid", "rio", &["small"]);
    wps_no_uid.metadata.uid = None;
    assert!(
        !is_wps_owned_by(&child_a, &wps_no_uid),
        "WPS without UID → can't prove ownership, don't prune"
    );
}
