//! Metric series ownership: declared registries replacing incidental
//! call sites.
//!
//! Three registries (C3 metric-ownership):
//!
//! - [`ALERT_SEEDED_COUNTERS`] — every PrometheusRule/ScaledObject
//!   `expr:`-referenced counter, born at 0 on every replica from
//!   [`crate::describe_metrics`] (the bug_322 birth-gap class: an
//!   alert like `sum(rate(...)) > 0` evaluates an ABSENT series until
//!   the first increment, so the first burst after a fresh rollout is
//!   invisible). Seeding lives here, not in `DagActor::new` — boot
//!   scrape-surface is a process property, not an actor property, and
//!   the standby/leader gauge tests' touch-sets stay untouched.
//!
//! - `leader_gauges!` / [`LeaderGauge`] — the leader-published state
//!   gauge family. One declaration carries name + label axis + reset
//!   value; `reset_leader_gauges()` derives the lose-edge sweep from
//!   the same rows, so a gauge added to the family CANNOT be missed by
//!   the loss reset (the merged_bug_025 class: the hand list in
//!   `handle_leader_lost` omitted `materialization_stalled`, leaving a
//!   deposed leader's frozen parked-count feeding the MD-D1 alert).
//!   Reset values are per-member because 0.0 is wrong for ratio
//!   gauges: `sla_prior_divergence`'s neutral is 1.0 — a 0.0 sweep
//!   would itself fire the clamp alert (`<= 0.5`).
//!
//! - [`LEADER_EDGES`] — paired acquire/lose hooks. Both
//!   `handle_leader_acquired` and `handle_leader_lost` iterate the
//!   SAME table, so an acquire-side effect cannot merge without its
//!   lose cell WRITTEN (no-ops are explicit and named). Closes
//!   bug_310: the cost-table edge-reload latch (`cost_was_leader`) had
//!   an acquire-side consumer but no lose-side writer, so an A→B→A
//!   lease flap inside one 600s housekeeping tick skipped the
//!   cost-table reload and the tick body persisted stale prices.
// r[impl obs.metric.alert-counter-seeded]

use std::sync::atomic::Ordering;

/// One boot-seeded counter family: bare name plus its closed label
/// axis. The shape mirrors `rio_test_support::metrics::SeededCounter`
/// so the alert-parity test consumes this exact table.
pub struct SeededSeries {
    pub name: &'static str,
    pub label: Option<(&'static str, &'static [&'static str])>,
}

/// Materialization job origins (the `origin` label's closed set).
pub const MAT_ORIGINS: &[&str] = &["pruned", "cache_opportunity", "stale_reset", "reprobe"];
/// Materialization job resolution outcomes (the `outcome` label).
pub const MAT_OUTCOMES: &[&str] = &[
    "success",
    "from_source",
    "unobtainable",
    "cancelled",
    "obsolete",
];
/// `sla_prior_divergence` parameter axis (prior.rs `clamp_field`
/// callers — the fit dims plus the scalar fit params).
pub const DIVERGENCE_PARAMS: &[&str] = &[
    "alpha_alu",
    "alpha_membw",
    "alpha_ioseq",
    "s",
    "p",
    "q",
    "a",
    "b",
];

/// Every alert-`expr:`-referenced rio_scheduler counter, plus the
/// T-6.2 materialization lifecycle movees (boot-visible for the VM
/// metrics-registered assertion even when never incremented). The
/// alert-parity test (`tests/alert_metrics.rs`) fails if a rule
/// references a counter missing here; the seed loop below births each
/// label-product series at 0.
pub const ALERT_SEEDED_COUNTERS: &[SeededSeries] = &[
    // bug_322: the ≥2-establishments-in-30m tripwire missed the
    // series-birth increments — `sum(increase(...))` over an absent
    // series is empty, not 0, so the first establishment burst after
    // a rollout never fired the alert.
    SeededSeries {
        name: "rio_scheduler_pull_establishments_total",
        label: None,
    },
    // Same birth-gap class (322 sibling): `sum(rate(...)) > 0` at
    // prometheusrule.yaml's fenced-write alert misses the first
    // fenced-write burst on a fresh series.
    SeededSeries {
        name: "rio_scheduler_evidence_write_fenced_total",
        label: None,
    },
    SeededSeries {
        name: "rio_scheduler_materialization_claims_total",
        label: None,
    },
    SeededSeries {
        name: "rio_scheduler_materialization_jobs_created_total",
        label: Some(("origin", MAT_ORIGINS)),
    },
    SeededSeries {
        name: "rio_scheduler_materialization_jobs_resolved_total",
        label: Some(("outcome", MAT_OUTCOMES)),
    },
    // Item T conversion counter — the RioSchedulerMaterializationConversions
    // alert matches {origin="cache_opportunity"}; the seeded product
    // covers every origin so the matcher has a series from boot.
    SeededSeries {
        name: "rio_scheduler_materialization_converted_total",
        label: Some(("origin", MAT_ORIGINS)),
    },
];

/// Birth every [`ALERT_SEEDED_COUNTERS`] series at 0. Called from
/// [`crate::describe_metrics`] — `rio_common::server` installs the
/// real exporter via `init_metrics` BEFORE `describe_metrics()`, so
/// the seeds land on the scrape surface on every replica (leader and
/// standby alike: counters are increment-owned, not leader-owned).
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

/// Declare the leader-published gauge family. Each row:
/// `Variant => (name, label_axis, reset_value)`. Generates the
/// [`LeaderGauge`] enum, `ALL`, accessors, and the family-driven
/// reset/seed sweeps.
macro_rules! leader_gauges {
    ( $( $variant:ident => ($name:literal, $axis:expr, $reset:literal) ),+ $(,)? ) => {
        /// The leader-published state gauge family. Publish through
        /// [`LeaderGauge::set`]/[`LeaderGauge::set_with`] (typed —
        /// the bidirectional grep test reconciles raw `gauge!` emits
        /// against this family plus the per-replica exemptions).
        #[derive(Clone, Copy, Debug, PartialEq, Eq)]
        pub enum LeaderGauge {
            $( $variant, )+
        }

        impl LeaderGauge {
            /// Every member, for family-driven sweeps and tests.
            pub const ALL: &'static [LeaderGauge] = &[ $( LeaderGauge::$variant, )+ ];

            /// The Prometheus series name.
            pub fn name(self) -> &'static str {
                match self {
                    $( LeaderGauge::$variant => $name, )+
                }
            }

            /// The closed label axis, when the member is labeled.
            pub fn label_axis(self) -> Option<(&'static str, &'static [&'static str])> {
                match self {
                    $( LeaderGauge::$variant => $axis, )+
                }
            }

            /// The lose-edge / boot-seed value. Per-member because 0.0
            /// is wrong for ratio gauges (divergence neutral = 1.0).
            pub fn reset_value(self) -> f64 {
                match self {
                    $( LeaderGauge::$variant => $reset, )+
                }
            }

            /// Set an unlabeled member. Debug-asserts the member has
            /// no axis (labeled members go through [`Self::set_with`]).
            pub fn set(self, v: f64) {
                debug_assert!(
                    self.label_axis().is_none(),
                    "{}: labeled gauge set without a label — use set_with",
                    self.name()
                );
                metrics::gauge!(self.name()).set(v);
            }

            /// Set one labeled series of this member.
            pub fn set_with(self, value: &'static str, v: f64) {
                let (axis, values) = self
                    .label_axis()
                    .expect("set_with on an unlabeled leader gauge");
                debug_assert!(
                    values.contains(&value),
                    "{}: label value {value:?} outside the declared axis {values:?}",
                    self.name()
                );
                metrics::gauge!(self.name(), axis => value).set(v);
            }

            /// Sweep this member to its declared reset value (every
            /// labeled series of the axis product).
            pub fn reset(self) {
                match self.label_axis() {
                    None => metrics::gauge!(self.name()).set(self.reset_value()),
                    Some((axis, values)) => {
                        for value in values {
                            metrics::gauge!(self.name(), axis => *value).set(self.reset_value());
                        }
                    }
                }
            }
        }
    };
}

leader_gauges! {
    DerivationsQueued => ("rio_scheduler_derivations_queued", None, 0.0),
    BuildsActive => ("rio_scheduler_builds_active", None, 0.0),
    DerivationsRunning => ("rio_scheduler_derivations_running", None, 0.0),
    SubstitutingDerivations => ("rio_scheduler_substituting_derivations", None, 0.0),
    OpenAttempts => ("rio_scheduler_open_attempts", None, 0.0),
    // A2.4's kind split (§4.R7): the materialization lane's own series
    // joins the family the moment it exists — the bidirectional grep
    // test forces membership.
    OpenMaterializationAttempts => ("rio_scheduler_open_materialization_attempts", None, 0.0),
    MaterializationStalled => ("rio_scheduler_materialization_stalled", None, 0.0),
    // Ratio gauge: neutral is 1.0 (in-band). A 0.0 sweep would read as
    // "clamped at the low edge" and fire RioSlaPriorDivergenceClamped
    // (`<= 0.5`) on every failover — the deposed-leader wedge this
    // family exists to prevent, inverted.
    SlaPriorDivergence =>
        ("rio_scheduler_sla_prior_divergence", Some(("param", DIVERGENCE_PARAMS)), 1.0),
}

/// Sweep every family member to its declared reset value. The single
/// lose-edge owner — `handle_leader_lost` reaches this through
/// [`LEADER_EDGES`]; nothing else writes a family gauge off-leader.
pub fn reset_leader_gauges() {
    for g in LeaderGauge::ALL {
        g.reset();
    }
}

/// Boot-seed the family at the same declared values. Makes the helm
/// comment ("every replica exports the leader gauges; non-leaders
/// hold the reset value") true from the first scrape: without the
/// seed, a standby that never led exports NOTHING and
/// `max()`/`min by (param)` reducers see only the leader — fine — but
/// a freshly-rolled fleet with no leader yet has NO series for the
/// stalled/divergence alerts to evaluate (the same birth-gap class as
/// the counters, gauge-shaped).
pub fn seed_leader_gauges() {
    reset_leader_gauges();
}

/// One paired leadership-edge effect: what runs on acquire, what runs
/// on lose. `handle_leader_acquired` and `handle_leader_lost` iterate
/// the SAME table — adding an acquire effect without writing its lose
/// cell does not compile (the cell is a struct field, not a
/// convention). No-ops are explicit (`|_| {}`) and the `name` says
/// what the pair owns.
pub struct LeaderEdge {
    pub name: &'static str,
    pub on_acquire: fn(&crate::actor::DagActor),
    pub on_lose: fn(&crate::actor::DagActor),
}

/// The leadership-edge effect table (bug_310's structural close).
///
/// Cost-table latch: WHY `store(false)` and not a lose-side notify —
/// `Notify` coalesces. A lose-notify followed by a re-acquire-notify
/// before housekeeping's select wakes collapses into ONE wake that
/// observes `is_leader && cost_was_leader` ⇒ skips the reload,
/// re-creating the bug. The direct false-store is wake-timing
/// independent: after ANY lose edge, the first leader housekeeping
/// prelude sees `cost_was_leader == false` and reloads the cost table
/// BEFORE the tick body persists (sla/cost.rs leader prelude); the
/// spot-price poller's own edge-skip re-arms the same way. False
/// stores are monotone-safe: a spurious extra reload is one PG read.
pub const LEADER_EDGES: &[LeaderEdge] = &[
    LeaderEdge {
        name: "cost-table-edge-reload-latch",
        // Verbatim the pre-table acquire effect: nudge housekeeping so
        // the false→true store + reload happen within ~0s of the lease
        // win, not ~600s (the controller's edge-detected instance-type
        // observations during that window would be dropped).
        on_acquire: |a| a.cost_reload_notify.notify_one(),
        // THE missing lose-edge writer (bug_310): without it, an
        // A→B→A flap within one housekeeping tick leaves
        // `cost_was_leader == true`, the prelude skips the reload, and
        // the tick persists prices from the deposed tenure.
        on_lose: |a| a.cost_was_leader.store(false, Ordering::Relaxed),
    },
    LeaderEdge {
        name: "leader-gauge-family",
        // No acquire effect: the next leader tick republishes every
        // member from ground truth (snapshot/sweep sites) — seeding
        // here would race the first tick for no benefit.
        on_acquire: |_| {},
        on_lose: |_| reset_leader_gauges(),
    },
];

#[cfg(test)]
mod tests {
    use super::*;
    use rio_test_support::metrics::CountingRecorder;

    /// `describe_metrics()` births every alert-seeded series at 0 on
    /// the installed recorder — the boot/scrape-surface property the
    /// VM metrics-registered assertion checks end-to-end. Pre-seed red
    /// (recorded in the introducing commit): pull_establishments_total
    /// and evidence_write_fenced_total were absent until their first
    /// production increment, so `sum(increase(...))`-shaped alerts
    /// evaluated an empty instant vector across the entire first
    /// burst.
    #[test]
    fn describe_metrics_births_alert_counters_at_zero() {
        let recorder = CountingRecorder::default();
        metrics::with_local_recorder(&recorder, crate::describe_metrics);
        let keys = recorder.all_keys();
        for s in ALERT_SEEDED_COUNTERS {
            match s.label {
                None => {
                    let key = format!("{}{{}}", s.name);
                    assert!(
                        keys.contains(&key),
                        "{} not born by describe_metrics() (keys: {keys:?})",
                        s.name
                    );
                    assert_eq!(recorder.get(&key), 0, "{key} must be born at 0");
                }
                Some((axis, values)) => {
                    for v in values {
                        let key = format!("{}{{{axis}={v}}}", s.name);
                        assert!(
                            keys.contains(&key),
                            "{key} not born by describe_metrics() (keys: {keys:?})"
                        );
                        assert_eq!(recorder.get(&key), 0, "{key} must be born at 0");
                    }
                }
            }
        }
    }

    /// The boot seed also births every leader-family gauge at its
    /// declared reset (gauge-shaped birth gap: a fleet with no leader
    /// yet has no series for the stalled/divergence alerts without
    /// this).
    #[test]
    fn describe_metrics_births_leader_gauges_at_reset() {
        let recorder = CountingRecorder::default();
        metrics::with_local_recorder(&recorder, crate::describe_metrics);
        for g in LeaderGauge::ALL {
            match g.label_axis() {
                None => assert_eq!(
                    recorder.gauge_value(&format!("{}{{}}", g.name())),
                    Some(g.reset_value()),
                    "{} must be born at its declared reset",
                    g.name()
                ),
                Some((axis, values)) => {
                    for v in values {
                        assert_eq!(
                            recorder.gauge_value(&format!("{}{{{axis}={v}}}", g.name())),
                            Some(g.reset_value()),
                            "{}{{{axis}={v}}} must be born at its declared reset",
                            g.name()
                        );
                    }
                }
            }
        }
    }

    /// Every LEADER_EDGES row is named and total — both cells written.
    /// (Totality is structural — fn-pointer fields can't be omitted —
    /// this pins the names stay meaningful and the table non-empty.)
    #[test]
    fn leader_edges_table_is_named_and_nonempty() {
        assert!(!LEADER_EDGES.is_empty());
        for e in LEADER_EDGES {
            assert!(!e.name.is_empty(), "LEADER_EDGES rows carry owner names");
        }
    }
}
