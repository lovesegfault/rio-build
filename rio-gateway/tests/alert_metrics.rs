//! Alert-parity policy test (merged_bug_236 adoption — see
//! rio-scheduler/tests/alert_metrics.rs for the pattern's origin).
//!
//! Consumes `rio_gateway::ALERT_SEEDED_COUNTERS` (the one-declaration
//! law: the seeder and this test read the same table). The founding
//! counter member is `rio_gateway_putpath_aborted_retries_total` —
//! referenced by the store ScaledObject's demand-side inhibitor
//! trigger, so its birth gap would blind the scale-down inhibitor
//! exactly during a quiet boot.
// r[verify obs.metric.alert-counter-seeded]

use rio_gateway::ALERT_SEEDED_COUNTERS;
use rio_test_support::metrics::{GaugeExemption, SeededCounter, assert_alert_metrics_covered};

/// Helm templates whose `expr:`/`query:` blocks may reference gateway
/// series. Paths are relative to the crate dir (nextest CWD).
const ALERT_YAMLS: &[&str] = &[
    "../infra/helm/rio-build/templates/prometheusrule.yaml",
    "../infra/helm/rio-build/templates/store-scaledobject.yaml",
    "../infra/helm/rio-build/templates/gateway-scaledobject.yaml",
];

fn gauge_exemptions() -> Vec<GaugeExemption> {
    vec![GaugeExemption {
        name: "rio_gateway_channels_active",
        rationale: "per-replica BY DESIGN: each gateway replica's own live \
                    session count is exactly the KEDA scaling signal — the \
                    ScaledObject sums across replicas; inc/dec are paired on \
                    accept/close so the series exists from the first session \
                    and 0 is a true reading, not a birth gap",
    }]
}

#[test]
fn alert_referenced_series_exist_from_boot() {
    let seeded: Vec<SeededCounter> = ALERT_SEEDED_COUNTERS
        .iter()
        .map(|s| SeededCounter {
            name: s.name,
            label: s.label,
        })
        .collect();
    let exemptions = gauge_exemptions();
    assert_alert_metrics_covered(
        ALERT_YAMLS,
        "rio_gateway_",
        rio_gateway::describe_metrics,
        &seeded,
        &[],
        &exemptions,
        "rio-gateway",
    );
}
