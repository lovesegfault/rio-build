//! Alert-parity + gauge-ownership policy tests (C3 metric-ownership).
//!
//! Two structural nets:
//!
//! 1. [`alert_referenced_series_exist_from_boot`] — every metric a
//!    PrometheusRule/ScaledObject `expr:` references must exist on the
//!    scrape surface from boot: counters via the
//!    `ALERT_SEEDED_COUNTERS` boot-seed table (bug_322's birth-gap
//!    class), gauges via the leader-gauge family or a
//!    rationale-carrying exemption (merged_bug_025's frozen-series
//!    class), histograms exempt by type. Reads the helm templates
//!    from the repo checkout — the nix fileset for this test's
//!    sandbox is wired in nix/lib/nextest-args.nix.
//!
//! 2. [`raw_gauge_emits_are_exactly_the_exemptions`] — single
//!    ownership, both directions: every raw `metrics::gauge!` literal
//!    in src/ is a declared exemption (family members publish ONLY
//!    through the typed `LeaderGauge` accessors — no literal exists
//!    for them outside the one declaration), and every exemption is
//!    actually emitted (no stale exemption rows).
// r[verify obs.metric.alert-counter-seeded]
// r[verify obs.metric.scheduler-leader-gate+5]

use rio_scheduler::observability::{ALERT_SEEDED_COUNTERS, LeaderGauge};
use rio_test_support::metrics::{
    GaugeExemption, SeededCounter, assert_alert_metrics_covered, grep_emitted_gauge_names,
};

/// Helm templates whose `expr:`/`query:` blocks reference scheduler
/// series. Paths are relative to the crate dir (nextest CWD).
const ALERT_YAMLS: &[&str] = &[
    "../infra/helm/rio-build/templates/prometheusrule.yaml",
    "../infra/helm/rio-build/templates/store-scaledobject.yaml",
    "../infra/helm/rio-build/templates/gateway-scaledobject.yaml",
];

/// Per-replica / own-edge gauges, exempt from the leader family. The
/// rationale strings are the documentation — surfaced verbatim when
/// the parity test fails.
fn gauge_exemptions() -> Vec<GaugeExemption> {
    vec![
        GaugeExemption {
            name: "rio_scheduler_actor_mailbox_depth",
            rationale: "per-replica by design: each replica's own actor mailbox; \
                        a standby's depth is real signal, not a stale leader copy",
        },
        GaugeExemption {
            name: "rio_scheduler_backpressure_projected_drain_seconds",
            rationale: "per-replica by design (round-9 B6): depth × per-turn cost \
                        EWMA of THIS replica's mailbox — the same ownership as \
                        actor_mailbox_depth, whose product it is; a standby's \
                        projection is its own real signal",
        },
        GaugeExemption {
            name: "rio_scheduler_sla_hw_cost_stale_seconds",
            rationale: "per-replica BY SPEC (observability.typ: climbs while this \
                        replica is standby under hw_cost_source=spot); zeroing it \
                        on lose would mask the staleness it measures",
        },
        GaugeExemption {
            name: "rio_scheduler_sla_class_ceiling_uncatalogued",
            rationale: "config-derived constant per replica (catalog vs [sla] \
                        ceilings); identical on every replica, no leader edge",
        },
        GaugeExemption {
            name: "rio_scheduler_status_outbox_depth",
            rationale: "own-edge-owned: clear_persisted_state() zeroes it with the \
                        outbox it measures (every clear caller, not just \
                        LeaderLost); family membership would double-own the reset",
        },
        GaugeExemption {
            name: "rio_scheduler_runtime_skew_seconds",
            rationale: "per-replica by design (sched.lease.guard-isolated): the \
                        guard-domain sentinel measures THIS replica's executor- \
                        scheduling delay, leader and standby alike; a standby's \
                        skew is its own real signal (the sh-002C 16.35s Tick \
                        attribution), no leader edge",
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
    let family: Vec<&str> = LeaderGauge::ALL.iter().map(|g| g.name()).collect();
    let exemptions = gauge_exemptions();
    assert_alert_metrics_covered(
        ALERT_YAMLS,
        "rio_scheduler_",
        rio_scheduler::describe_metrics,
        &seeded,
        &family,
        &exemptions,
        "rio-scheduler",
    );
}

#[test]
fn raw_gauge_emits_are_exactly_the_exemptions() {
    let manifest_dir =
        std::env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR set by cargo/nextest");
    let raw: Vec<String> = grep_emitted_gauge_names(&manifest_dir)
        .into_iter()
        .filter(|n| n.starts_with("rio_scheduler_"))
        .collect();
    let exemptions = gauge_exemptions();

    // Direction 1: every raw emit is exempted. A family member here
    // means a publish site bypassed the typed accessors.
    for name in &raw {
        assert!(
            exemptions.iter().any(|e| e.name == name),
            "raw metrics::gauge!({name:?}) emit in src/ is not an exempted \
             per-replica gauge — leader-family members publish only through \
             LeaderGauge::set/set_with (single ownership). raw set: {raw:?}"
        );
        assert!(
            !LeaderGauge::ALL.iter().any(|g| g.name() == name),
            "{name}: leader-family member emitted via a raw gauge! literal — \
             use the typed accessor"
        );
    }

    // Direction 2: every exemption row is real (still emitted).
    for e in &exemptions {
        assert!(
            raw.iter().any(|n| n == e.name),
            "stale exemption: {} has no raw gauge! emit left in src/ — drop the \
             row or re-point it. raw set: {raw:?}",
            e.name
        );
    }
}
