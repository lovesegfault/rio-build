//! Alert-parity policy test (merged_bug_236 — the C3 metric-ownership
//! net extended to the controller).
//!
//! Every metric a PrometheusRule/ScaledObject `expr:` references must
//! exist on the scrape surface from boot: counters via the
//! `ALERT_SEEDED_COUNTERS` boot-seed table (bug_322's birth-gap
//! class — `increase(...) > 0` over an absent series is empty, not 0),
//! gauges via a rationale-carrying exemption (the controller runs as a
//! single replica with no leader-gauge family — every exemption says
//! so explicitly), histograms exempt by type. Reads the helm templates
//! from the repo checkout — the nix fileset for this test's sandbox is
//! wired in nix/lib/nextest-args.nix.
// r[verify obs.metric.alert-counter-seeded]

use rio_controller::observability::ALERT_SEEDED_COUNTERS;
use rio_test_support::metrics::{GaugeExemption, SeededCounter, assert_alert_metrics_covered};

/// Helm templates whose `expr:`/`query:` blocks may reference
/// controller series. Paths are relative to the crate dir (nextest
/// CWD).
const ALERT_YAMLS: &[&str] = &[
    "../infra/helm/rio-build/templates/prometheusrule.yaml",
    "../infra/helm/rio-build/templates/store-scaledobject.yaml",
    "../infra/helm/rio-build/templates/gateway-scaledobject.yaml",
];

/// Controller gauges referenced by alerts, exempt from any
/// leader-family requirement. The shared rationale: the controller is
/// a replicas=1 deployment (one leader-elected pod; the standby case
/// the scheduler's leader-gauge family exists for does not arise), so
/// a per-replica gauge IS the component truth; each is also
/// freshness-bounded by its own emit cadence.
fn gauge_exemptions() -> Vec<GaugeExemption> {
    vec![
        GaugeExemption {
            name: "rio_controller_nodeclaim_inflight_age_max_seconds",
            rationale: "replicas=1: the single reconciler's own in-flight view is \
                        the component truth (no standby replica to publish a \
                        frozen copy); re-published every 10s tick, 0 when none",
        },
        GaugeExemption {
            name: "rio_controller_nodeclaim_ice_timeout_seconds",
            rationale: "replicas=1 + config-derived constant: the alert reads it \
                        as a threshold operand (clamp(2x, ...)), not as state; \
                        identical on any replica by construction",
        },
        GaugeExemption {
            name: "rio_controller_nodeclaim_terminating_age_max_seconds",
            rationale: "replicas=1: single reconciler's own terminating-claim \
                        view; re-published every 10s tick, 0 when none",
        },
    ]
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
        "rio_controller_",
        rio_controller::describe_metrics,
        &seeded,
        &[],
        &exemptions,
        "rio-controller",
    );
}
