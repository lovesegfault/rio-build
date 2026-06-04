//! Alert-parity policy test (merged_bug_236 adoption — see
//! rio-scheduler/tests/alert_metrics.rs for the pattern's origin).
//!
//! The gateway's only alert-surface metric today is the
//! `rio_gateway_channels_active` ScaledObject query; this adoption
//! file is what the `alert-parity-adoption` misc-check requires so a
//! FUTURE gateway alert cannot reference an unseeded counter or an
//! unowned gauge without failing here.
// r[verify obs.metric.alert-counter-seeded]

use rio_test_support::metrics::{GaugeExemption, SeededCounter, assert_alert_metrics_covered};

/// Helm templates whose `expr:`/`query:` blocks may reference gateway
/// series. Paths are relative to the crate dir (nextest CWD).
const ALERT_YAMLS: &[&str] = &[
    "../infra/helm/rio-build/templates/prometheusrule.yaml",
    "../infra/helm/rio-build/templates/store-scaledobject.yaml",
    "../infra/helm/rio-build/templates/gateway-scaledobject.yaml",
];

/// No gateway counters are alert-referenced yet — the empty table is
/// the assertion that none slipped in unseeded.
const SEEDED: &[SeededCounter] = &[];

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
    let exemptions = gauge_exemptions();
    assert_alert_metrics_covered(
        ALERT_YAMLS,
        "rio_gateway_",
        rio_gateway::describe_metrics,
        SEEDED,
        &[],
        &exemptions,
        "rio-gateway",
    );
}
