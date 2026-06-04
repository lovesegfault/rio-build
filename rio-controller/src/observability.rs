//! Metric series ownership: boot-seeded alert series (merged_bug_236 —
//! the rio-scheduler C3 pattern extended to the controller).
//!
//! Every PrometheusRule `expr:`-referenced controller counter is born
//! at 0 from [`crate::describe_metrics`] (the bug_322 birth-gap class:
//! `increase(...) > 0` and `count(sum by (cell)(rate(...)) > 0)`
//! evaluate an ABSENT series until the first increment, so the first
//! drop/reap burst after a fresh rollout is invisible to the alert).
//!
//! Two seeding planes:
//!
//! - [`ALERT_SEEDED_COUNTERS`] — static label products, seeded
//!   unconditionally at boot ([`seed_alert_counters`]).
//! - [`seed_reaped_cells`] — `rio_controller_nodeclaim_reaped_total`
//!   carries a config-derived `cell` axis the static table cannot
//!   know; the `by (cell)` alert needs per-cell series birth, so the
//!   reasons × live-cell product is seeded at `HwClassConfig`
//!   load/refresh (`.absolute(0)` is idempotent — re-seeding on every
//!   300 s refresh is free).
//!
//! The alert-parity test (`tests/alert_metrics.rs`) fails if a rule
//! references a counter missing from the table; the
//! `alert-parity-adoption` misc-check fails if any component's metrics
//! reach a shipped alert expr without that component carrying the
//! parity test at all — the CLASS chokepoint that makes "new
//! component, seeded alerts forgotten" CI-red instead of a silent
//! birth gap.
// r[impl obs.metric.alert-counter-seeded]

/// One boot-seeded counter family: bare name plus its closed label
/// axis. Mirrors `rio_test_support::metrics::SeededCounter` so the
/// parity test consumes this exact table.
pub struct SeededSeries {
    pub name: &'static str,
    pub label: Option<(&'static str, &'static [&'static str])>,
}

/// `rio_controller_nodeclaim_reaped_total`'s closed `reason` set
/// (health::ReapReason::as_str ∪ the vanished/idle emit sites).
pub const REAP_REASONS: &[&str] = &["ice", "boot-timeout", "dead", "vanished", "idle"];

/// `rio_controller_nodeclaim_intent_dropped_total`'s closed `reason`
/// set (cover sizing, pool-coverage retain, hosting-class resolve,
/// ICE-mask exhaustion, ceiling lookup).
pub const INTENT_DROP_REASONS: &[&str] = &[
    "all_cells_ice_masked",
    "exceeds_cell_cap",
    "no_hosting_class",
    "no_pool_covers",
    "unknown_hw_class",
];

/// Every alert-`expr:`-referenced rio_controller counter. The
/// `reaped_total` entry's static seed births the reason axis only —
/// the cell-crossed series the `by (cell)` alert groups on are born by
/// [`seed_reaped_cells`] when the config arrives.
pub const ALERT_SEEDED_COUNTERS: &[SeededSeries] = &[
    SeededSeries {
        name: "rio_controller_nodeclaim_intent_dropped_total",
        label: Some(("reason", INTENT_DROP_REASONS)),
    },
    SeededSeries {
        name: "rio_controller_nodeclaim_reaped_total",
        label: Some(("reason", REAP_REASONS)),
    },
];

/// Birth every [`ALERT_SEEDED_COUNTERS`] series at 0. Called from
/// [`crate::describe_metrics`] — `rio_common::server` installs the
/// real exporter via `init_metrics` BEFORE `describe_metrics()`, so
/// the seeds land on the scrape surface from boot.
pub fn seed_alert_counters() {
    for s in ALERT_SEEDED_COUNTERS {
        match s.label {
            None => metrics::counter!(s.name).absolute(0),
            Some((axis, values)) => {
                for v in values {
                    metrics::counter!(s.name, axis => *v).absolute(0);
                }
            }
        }
    }
}

/// Birth the `reasons × cells` product for
/// `rio_controller_nodeclaim_reaped_total` — the
/// `count(sum by (cell)(rate(...)) > 0) >= 3` multi-cell ICE alert
/// groups by `cell`, so each (reason, cell) series must exist from the
/// moment the cell is configured, not from its first reap. Called at
/// `HwClassConfig` load/refresh with the live cell set; `.absolute(0)`
/// is idempotent so refresh re-seeding is free and never resets a
/// counted value.
pub fn seed_reaped_cells<I: IntoIterator<Item = String>>(cells: I) {
    for cell in cells {
        for reason in REAP_REASONS {
            metrics::counter!(
                "rio_controller_nodeclaim_reaped_total",
                "reason" => *reason,
                "cell" => cell.clone(),
            )
            .absolute(0);
        }
    }
}
