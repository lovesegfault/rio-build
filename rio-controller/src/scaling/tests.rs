//! Unit tests for the shared scaling surface: queue-depth lookups
//! and the `is_wps_owned_by` ownership predicate.

use super::*;
use rio_crds::builderpool::BuilderPool;
use rio_crds::builderpoolset::BuilderPoolSet;

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

// ---- class_queued_for_systems: I-143 per-arch × per-class filter ----

/// I-143: an x86-64 size-class pool MUST NOT spawn for aarch64
/// derivations in the same class. Without the per-system breakdown,
/// `queued=2042` (all aarch64) drove the x86 pool to ceiling — half
/// the cluster's builders idle-timed-out and respawned forever.
// r[verify ctrl.pool.per-system-class-depth]
#[test]
fn class_queued_for_systems_ignores_other_arch() {
    use rio_proto::types::SizeClassStatus;
    let class = SizeClassStatus {
        name: "tiny".into(),
        queued: 100,
        queued_by_system: [("aarch64-linux".into(), 100)].into(),
        ..Default::default()
    };

    // The bug: x86 pool reads class-wide queued=100. The fix: 0.
    assert_eq!(
        class_queued_for_systems(&class, &["x86_64-linux".into()]),
        0,
        "x86 pool sees no work when all 100 queued are aarch64"
    );
    // The matching pool DOES see the work.
    assert_eq!(
        class_queued_for_systems(&class, &["aarch64-linux".into()]),
        100
    );

    // Mixed: each pool sees its own slice; multi-system pool sums.
    let mixed = SizeClassStatus {
        queued: 105,
        queued_by_system: [("x86_64-linux".into(), 100), ("aarch64-linux".into(), 5)].into(),
        ..Default::default()
    };
    assert_eq!(
        class_queued_for_systems(&mixed, &["aarch64-linux".into()]),
        5
    );
    assert_eq!(
        class_queued_for_systems(&mixed, &["x86_64-linux".into(), "aarch64-linux".into()]),
        105,
        "multi-system pool sums its entries"
    );

    // Backward compat: empty map (old scheduler) → scalar.
    let old = SizeClassStatus {
        queued: 42,
        ..Default::default()
    };
    assert_eq!(
        class_queued_for_systems(&old, &["x86_64-linux".into()]),
        42,
        "old scheduler (no breakdown) → fall back to class scalar"
    );
    // Empty systems → scalar (nothing to filter on).
    assert_eq!(class_queued_for_systems(&mixed, &[]), 105);
}

// r[verify ctrl.fetcherpool.spawn-builtin]
/// Latent cold-store stall: `compute_fod_size_class_snapshot` keys by
/// `drv.system`; bootstrap-tools FODs have `system="builtin"`. A pool
/// with `spec.systems=[x86_64-linux]` (the pre-multiarch default)
/// would see `class_queued_for_systems → 0` and never spawn — the FOD
/// queues forever. Unobserved in production because bootstrap FODs are
/// warm-cached after first run, but a fresh store would deadlock.
///
/// Fix: `"builtin"` is summed regardless of `spec.systems`. With
/// per-arch pools both listing `[<arch>, builtin]` this means ≤2×
/// spawn for `builtin` work — cheap (`tiny` = 500m/1Gi, 300s TTL) and
/// `reap_excess_pending` reclaims the surplus.
#[test]
fn class_queued_counts_builtin() {
    use rio_proto::types::SizeClassStatus;
    let class = SizeClassStatus {
        name: "tiny".into(),
        queued: 8,
        queued_by_system: [("builtin".into(), 5), ("x86_64-linux".into(), 3)].into(),
        ..Default::default()
    };

    // The latent bug: pool didn't list `builtin` → must still count it.
    assert_eq!(
        class_queued_for_systems(&class, &["x86_64-linux".into()]),
        8,
        "builtin FODs counted even when spec.systems omits it"
    );
    // Pool that DOES list it (chart default) → no double-count.
    assert_eq!(
        class_queued_for_systems(&class, &["x86_64-linux".into(), "builtin".into()]),
        8,
        "explicit builtin in spec.systems doesn't double-count"
    );
    // Per-arch overflow: aarch64 pool sees builtin work (5), not x86 (3).
    assert_eq!(
        class_queued_for_systems(&class, &["aarch64-linux".into(), "builtin".into()]),
        5,
        "aarch64 pool sees builtin (overflow) but not x86_64-linux FODs"
    );
    // builtin-only backlog, arm-only pool, builtin omitted from spec
    // → still spawns (the safety-net path).
    let bonly = SizeClassStatus {
        queued: 5,
        queued_by_system: [("builtin".into(), 5)].into(),
        ..Default::default()
    };
    assert_eq!(
        class_queued_for_systems(&bonly, &["aarch64-linux".into()]),
        5
    );
}

// ---- class_queued_for_pool: I-176 per-feature × per-class filter ----

/// I-176: a kvm derivation classified as `tiny` (trivial runCommand
/// wrapping a VM test) MUST trigger a spawn from the kvm pool — even
/// if the kvm pool's own `sizeClass` is `xlarge` — and MUST NOT
/// trigger a spawn from the featureless tiny pool. The response here
/// is what the scheduler returns AFTER feature filtering (the
/// controller passed `pool_features` in the request); this test covers
/// the controller-side cross-class sum + featureless single-class
/// lookup that turn that response into a spawn count.
// r[verify ctrl.pool.per-feature-class-depth]
#[test]
fn class_queued_for_pool_feature_aware() {
    use rio_proto::types::{GetSizeClassStatusResponse, SizeClassStatus};

    let x86 = || vec!["x86_64-linux".to_string()];

    // --- Featureless general pool, sizeClass="tiny", features=[] ---
    // Controller sent `filter_features=true, pool_features=[]` →
    // scheduler excluded the kvm derivation. Response: tiny.queued=0.
    // Single-class lookup (features empty → no cross-class sum).
    let resp_featureless = GetSizeClassStatusResponse {
        classes: vec![
            SizeClassStatus {
                name: "tiny".into(),
                queued: 0,
                queued_by_system: [("x86_64-linux".into(), 0)].into(),
                ..Default::default()
            },
            SizeClassStatus {
                name: "xlarge".into(),
                queued: 0,
                ..Default::default()
            },
        ],
        ..Default::default()
    };
    assert_eq!(
        class_queued_for_pool(&resp_featureless, "tiny", &x86(), &[]),
        Some(0),
        "I-176: featureless pool sees 0 — no wasted spawn for kvm work"
    );

    // --- kvm pool, sizeClass="xlarge", features=["kvm","nixos-test"] ---
    // Controller sent `pool_features=["kvm","nixos-test"]` → scheduler
    // counted the kvm derivation (it classifies as tiny). Response:
    // tiny.queued=1, xlarge.queued=0. WITHOUT cross-class sum, the
    // kvm pool would read xlarge=0 → never spawn → deadlock (the
    // featureless tiny pool's spawn gets `feature-missing`). WITH it:
    // sum across classes = 1 → spawn 1.
    let resp_kvm = GetSizeClassStatusResponse {
        classes: vec![
            SizeClassStatus {
                name: "tiny".into(),
                queued: 1,
                queued_by_system: [("x86_64-linux".into(), 1)].into(),
                ..Default::default()
            },
            SizeClassStatus {
                name: "xlarge".into(),
                queued: 0,
                queued_by_system: [("x86_64-linux".into(), 0)].into(),
                ..Default::default()
            },
        ],
        ..Default::default()
    };
    let kvm_features = vec!["kvm".to_string(), "nixos-test".to_string()];
    assert_eq!(
        class_queued_for_pool(&resp_kvm, "xlarge", &x86(), &kvm_features),
        Some(1),
        "I-176: kvm pool sums across classes — overflow will route the \
         tiny-classified kvm drv here; it MUST spawn"
    );

    // I-143 still applies inside the cross-class sum: aarch64 entries
    // don't count toward an x86 kvm pool.
    let resp_mixed_arch = GetSizeClassStatusResponse {
        classes: vec![SizeClassStatus {
            name: "tiny".into(),
            queued: 6,
            queued_by_system: [("x86_64-linux".into(), 1), ("aarch64-linux".into(), 5)].into(),
            ..Default::default()
        }],
        ..Default::default()
    };
    assert_eq!(
        class_queued_for_pool(&resp_mixed_arch, "xlarge", &x86(), &kvm_features),
        Some(1),
        "cross-class sum still intersects with pool systems (I-143)"
    );

    // Featureless pool, class missing from response → None (caller
    // falls through to cluster_status). Feature-gated pool, empty
    // response → also None.
    let empty = GetSizeClassStatusResponse::default();
    assert_eq!(
        class_queued_for_pool(&resp_kvm, "nope", &x86(), &[]),
        None,
        "featureless: unknown class → fallthrough"
    );
    assert_eq!(
        class_queued_for_pool(&empty, "xlarge", &x86(), &kvm_features),
        None,
        "feature-gated: size-classes off → fallthrough"
    );
}

// ---- is_wps_owned_by ----

fn test_wps(name: &str, ns: &str, class_names: &[&str]) -> BuilderPoolSet {
    use k8s_openapi::api::core::v1::ResourceRequirements;
    use rio_crds::builderpoolset::{BuilderPoolSetSpec, PoolTemplate, SizeClassSpec};
    let classes = class_names
        .iter()
        .map(|n| SizeClassSpec {
            common: rio_crds::common::SizeClassCommon {
                name: (*n).to_string(),
                max_concurrent: Some(10),
                resources: ResourceRequirements::default(),
            },
            cutoff_secs: 60.0,
        })
        .collect();
    let spec = BuilderPoolSetSpec {
        classes,
        pool_template: PoolTemplate {
            image: "rio-builder:test".into(),
            systems: vec!["x86_64-linux".into()],
            ..Default::default()
        },
    };
    let mut wps = BuilderPoolSet::new(name, spec);
    wps.metadata.uid = Some(format!("{name}-uid"));
    wps.metadata.namespace = Some(ns.into());
    wps
}

fn test_wp_in_ns(name: &str, ns: &str) -> BuilderPool {
    let mut wp = BuilderPool::new(name, crate::fixtures::test_builderpool_spec());
    wp.metadata.namespace = Some(ns.into());
    wp
}

fn with_wps_owner(mut wp: BuilderPool, wps_name: &str, wps_uid: &str) -> BuilderPool {
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

/// Two BuilderPoolSets in the same namespace; each stamps a child
/// pool with a `controller=true` ownerReference. Then one WPS
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
