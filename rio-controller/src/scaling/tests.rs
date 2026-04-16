//! Unit tests for the shared scaling surface: queue-depth lookups.

use super::*;

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

// I-143 / I-176 / I-181 filtering moved server-side
// (`compute_spawn_intents` request filter); semantics covered in
// rio-scheduler `actor/tests/misc.rs::spawn_intents_feature_filter`.
