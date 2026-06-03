//! Store alert-parity test (C3 metric-ownership) — the rio-scheduler
//! twin (see rio-scheduler/tests/alert_metrics.rs for the rule
//! mechanics). Every rio_store metric a PrometheusRule `expr:`
//! references must exist from boot: counters via the
//! ALERT_SEEDED_COUNTERS table, gauges via self-publication ownership
//! (the store has no leader concept — every store gauge is per-replica
//! and boot-seeded in describe_metrics; the family argument is the
//! boot-seeded gauge set), histograms exempt by type.
// r[verify obs.metric.alert-counter-seeded]

use rio_store::{ALERT_SEEDED_COUNTERS, describe_metrics};
use rio_test_support::metrics::{GaugeExemption, SeededCounter, assert_alert_metrics_covered};

const ALERT_YAMLS: &[&str] = &[
    "../infra/helm/rio-build/templates/prometheusrule.yaml",
    "../infra/helm/rio-build/templates/store-scaledobject.yaml",
    "../infra/helm/rio-build/templates/gateway-scaledobject.yaml",
];

#[test]
fn alert_referenced_series_exist_from_boot() {
    let seeded: Vec<SeededCounter> = ALERT_SEEDED_COUNTERS
        .iter()
        .map(|s| SeededCounter {
            name: s.name,
            label: s.label,
        })
        .collect();
    // Store gauges are per-replica (no leader gate); the ones an alert
    // may reference are all boot-seeded in describe_metrics() and
    // periodically self-published (obs.metric.store-gauge-ownership).
    // Pass them as the "family" set — the parity property is the same:
    // the series exists from boot and cannot freeze.
    let self_published: Vec<&str> = vec![
        "rio_store_pg_pool_utilization",
        "rio_store_substitute_admission_utilization",
        "rio_store_gc_sweep_paths_remaining",
        "rio_store_gc_chunks_live",
        "rio_store_gc_chunks_would_collect",
        "rio_store_gc_collect_backlog_chunks",
        "rio_store_s3_deletes_pending",
        "rio_store_s3_deletes_stuck",
        "rio_store_log_active_ingest_sessions",
        "rio_store_log_tail_subscribers",
    ];
    let exemptions: Vec<GaugeExemption> = vec![];
    assert_alert_metrics_covered(
        ALERT_YAMLS,
        "rio_store_",
        describe_metrics,
        &seeded,
        &self_published,
        &exemptions,
        "rio-store",
    );
}
