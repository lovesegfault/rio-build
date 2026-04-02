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
use crate::crds::builderpool::BuilderPool;
use crate::crds::builderpoolset::BuilderPoolSet;

// ---- sts_replicas_patch: SSA body shape ----

/// SSA patches MUST carry apiVersion + kind, or the apiserver
/// returns 400 "apiVersion must be set". A body like
/// `{"spec":{"replicas":N}}` silently fails every scale. This
/// test is a tripwire: if someone strips the GVK fields "for
/// brevity," this breaks.
/// BuilderPool status patch needs apiVersion+kind (same SSA
/// requirement as the STS replicas patch). Also verify it's a
/// PARTIAL status: replicas/ready/desired absent (reconciler's
/// fields), lastScaleTime + conditions present (ours).
#[test]
fn wp_status_patch_has_gvk_and_partial_status() {
    let cond = scaling_condition("True", "ScaledUp", "from 1 to 3", None);
    let patch = wp_status_patch(std::slice::from_ref(&cond));

    // GVK: SSA rejects without these.
    assert!(
        patch.get("apiVersion").and_then(|v| v.as_str()).is_some(),
        "SSA body without apiVersion → 400"
    );
    assert_eq!(
        patch.get("kind").and_then(|v| v.as_str()),
        Some("BuilderPool")
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
    let c = scaling_condition("False", "UnknownMetric", "metric 'foo' unsupported", None);
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

/// `lastTransitionTime` is preserved when status is unchanged
/// (K8s convention: timestamp changes on True↔False transition,
/// not on every write). Without this, ephemeral.rs's 10s requeue
/// makes the field always read "~10s ago" — useless for "when
/// did the scheduler go down."
#[test]
fn scaling_condition_preserves_timestamp_on_same_status() {
    let prev = serde_json::json!({
        "type": "Scaling",
        "status": "True",
        "reason": "ScaledUp",
        "message": "from 1 to 3",
        "lastTransitionTime": "2026-01-01T00:00:00Z",
    });

    // Same status → preserve timestamp. Message/reason changed
    // (from 3→5) but status didn't transition.
    let c = scaling_condition("True", "ScaledUp", "from 3 to 5", Some(&prev));
    assert_eq!(
        c["lastTransitionTime"].as_str(),
        Some("2026-01-01T00:00:00Z"),
        "same status → preserve prev lastTransitionTime"
    );

    // Different status → stamp now(). This IS a transition.
    let c = scaling_condition("False", "UnknownMetric", "bad metric", Some(&prev));
    assert_ne!(
        c["lastTransitionTime"].as_str(),
        Some("2026-01-01T00:00:00Z"),
        "status transition (True→False) must stamp fresh timestamp"
    );

    // No prev → stamp now() (first write).
    let c = scaling_condition("True", "ScaledUp", "from 1 to 3", None);
    assert_ne!(
        c["lastTransitionTime"].as_str(),
        Some("2026-01-01T00:00:00Z"),
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

// ---- queued_for_systems: I-107 per-arch filter ----

#[test]
fn queued_for_systems_filters_by_pool_systems() {
    use rio_proto::types::ClusterStatusResponse;
    let status = ClusterStatusResponse {
        queued_derivations: 105,
        queued_by_system: [("x86_64-linux".into(), 100), ("aarch64-linux".into(), 5)].into(),
        ..Default::default()
    };
    // The directive's case: aarch64 pool sees 5, not 105.
    assert_eq!(
        queued_for_systems(&status, &["aarch64-linux".into()]),
        5,
        "aarch64 pool scales on aarch64 backlog only"
    );
    assert_eq!(queued_for_systems(&status, &["x86_64-linux".into()]), 100);
    // Multi-system pool sums its entries.
    assert_eq!(
        queued_for_systems(&status, &["x86_64-linux".into(), "builtin".into()]),
        100,
        "missing key (builtin) contributes 0"
    );
    // Backward compat: empty map → scalar.
    let old = ClusterStatusResponse {
        queued_derivations: 42,
        ..Default::default()
    };
    assert_eq!(
        queued_for_systems(&old, &["aarch64-linux".into()]),
        42,
        "old scheduler (empty map) → fall back to scalar"
    );
    // Empty systems → scalar (nothing to filter on).
    assert_eq!(queued_for_systems(&status, &[]), 105);
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
/// `controller=true` BuilderPoolSet owner reference, false
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
    let standalone = BuilderPool::new("standalone-pool", spec.clone());
    assert!(
        !is_wps_owned(&standalone),
        "no ownerRef → NOT WPS-owned (standalone pools must be scaled by the cluster-wide loop)"
    );

    // WPS child: ownerRef kind=BuilderPoolSet, controller=true.
    // Mirrors what builders::build_child_builderpool sets via
    // controller_owner_ref(&()).
    let mut wps_child = BuilderPool::new("test-wps-small", spec.clone());
    wps_child.metadata.owner_references = Some(vec![OwnerReference {
        api_version: "rio.build/v1alpha1".into(),
        kind: "BuilderPoolSet".into(),
        name: "test-wps".into(),
        uid: "wps-uid-456".into(),
        controller: Some(true),
        block_owner_deletion: Some(true),
    }]);
    assert!(
        is_wps_owned(&wps_child),
        "BuilderPoolSet ownerRef with controller=true → WPS-owned"
    );

    // Owned by something ELSE (e.g., a Helm chart adopting a
    // BuilderPool). controller=true but wrong kind.
    let mut other_owned = BuilderPool::new("other-owned", spec.clone());
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
        "non-BuilderPoolSet ownerRef → NOT WPS-owned (kind check is load-bearing)"
    );

    // WPS ownerRef but controller=None (garbage-collection
    // owner, not controller). The reconciler always sets
    // controller=true; this case shouldn't happen but
    // defensive: controller=None means NOT controller-owned.
    let mut gc_only = BuilderPool::new("gc-only", spec);
    gc_only.metadata.owner_references = Some(vec![OwnerReference {
        api_version: "rio.build/v1alpha1".into(),
        kind: "BuilderPoolSet".into(),
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
// builders.rs (unreachable cross-module). BuilderPoolSpec shape
// is pulled from crate::fixtures::test_workerpool_spec — the
// single touch point for E0063 on field adds.
//
// pub(super) so per_class.rs's tests can reuse them.

pub(super) fn test_wps(name: &str, ns: &str, class_names: &[&str]) -> BuilderPoolSet {
    use crate::crds::builderpoolset::{BuilderPoolSetSpec, PoolTemplate, SizeClassSpec};
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
    let spec = BuilderPoolSetSpec {
        classes,
        pool_template: PoolTemplate {
            image: "rio-builder:test".into(),
            systems: vec!["x86_64-linux".into()],
            ..Default::default()
        },
        cutoff_learning: None,
    };
    let mut wps = BuilderPoolSet::new(name, spec);
    wps.metadata.uid = Some(format!("{name}-uid"));
    wps.metadata.namespace = Some(ns.into());
    wps
}

pub(super) fn test_wp_in_ns(name: &str, ns: &str) -> BuilderPool {
    let mut wp = BuilderPool::new(name, crate::fixtures::test_workerpool_spec());
    wp.metadata.namespace = Some(ns.into());
    wp
}

pub(super) fn with_wps_owner(mut wp: BuilderPool, wps_name: &str, wps_uid: &str) -> BuilderPool {
    use k8s_openapi::apimachinery::pkg::apis::meta::v1::OwnerReference;
    wp.metadata.owner_references = Some(vec![OwnerReference {
        api_version: "rio.build/v1alpha1".into(),
        kind: "BuilderPoolSet".into(),
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

// ---- FetcherPool: fp_status_patch / fp_pool_key ----

/// FetcherPool status patch needs apiVersion+kind same as the
/// BuilderPool variant. Same SSA constraint, different GVK.
#[test]
fn fp_status_patch_has_gvk_and_partial_status() {
    let cond = scaling_condition("True", "ScaledUp", "from 1 to 3", None);
    let patch = fp_status_patch(std::slice::from_ref(&cond));

    assert!(
        patch.get("apiVersion").and_then(|v| v.as_str()).is_some(),
        "SSA body without apiVersion → 400"
    );
    assert_eq!(
        patch.get("kind").and_then(|v| v.as_str()),
        Some("FetcherPool"),
        "GVK is FetcherPool, not BuilderPool — wrong kind would 404"
    );

    let status = patch.get("status").expect("status key");
    assert!(status.get("lastScaleTime").is_some());
    // Reconciler's fields absent: same SSA field-ownership split.
    assert!(status.get("readyReplicas").is_none());
    assert!(status.get("desiredReplicas").is_none());
}

/// `fp_pool_key` is `fp:`-prefixed so the autoscaler's `states`
/// HashMap can hold both pool kinds without collision. A
/// BuilderPool and FetcherPool both named `rio` in the same
/// namespace MUST get separate stabilization windows — sharing
/// one would cause them to fight (BuilderPool's desired=10 then
/// FetcherPool's desired=2 would each reset the other's
/// `stable_since`).
#[test]
fn fp_pool_key_disjoint_from_builder_pool_key() {
    use crate::crds::builderpool::{Autoscaling, Replicas};
    use crate::crds::fetcherpool::{FetcherPool, FetcherPoolSpec};

    let bp = test_wp_in_ns("rio", "rio-builders");
    let mut fp = FetcherPool::new(
        "rio",
        FetcherPoolSpec {
            ephemeral: false,
            ephemeral_deadline_seconds: None,
            replicas: Replicas { min: 1, max: 4 },
            autoscaling: Autoscaling {
                metric: "fodQueueDepth".into(),
                target_value: 5,
            },
            image: "rio-fetcher:test".into(),
            systems: vec!["x86_64-linux".into()],
            node_selector: None,
            tolerations: None,
            resources: None,
            tls_secret_name: None,
            host_users: None,
        },
    );
    // Same namespace as the BuilderPool — would collide on
    // `ns/name` alone.
    fp.metadata.namespace = Some("rio-builders".into());

    let bp_key = pool_key(&bp);
    let fp_key = fp_pool_key(&fp);
    assert_ne!(
        bp_key, fp_key,
        "same ns/name MUST produce distinct state keys; got '{bp_key}' for both"
    );
    assert!(
        fp_key.starts_with("fp:"),
        "prefix is the discriminator; got '{fp_key}'"
    );
}
