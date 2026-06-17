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
//!
//! Formal-coverage record (directive-2, relocated from the retired
//! observability invariant map): series-lifecycle plumbing (Prometheus
//! birth semantics, metrics-rs registration, RPC-caller topology) has
//! no adversarial interleaving the registry types do not foreclose —
//! the binding enforcement is the parity tests, the bidirectional
//! gauge-policy test and the family-driven sweeps; a fresh model would
//! re-state the LEADER_EDGES table row by row. Partial supersession:
//! the cost-latch half of that disposition was later overturned by the
//! merged_bug_212 rebound-edge finding — `costLatch.qnt` now models the
//! latch protocol (the registry's cost-latch cells mirrored
//! structurally); the series-lifecycle half stands.
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
/// Every disposition `handle_leader_acquired` records on
/// `rio_scheduler_recovery_total` (one increment per attempt).
const RECOVERY_OUTCOMES: &[&str] = &[
    "success",
    "failure",
    "discarded_flap",
    "discarded_unconfirmed",
];

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
    // bug_155: the RioSchedulerRecoveryFailing alert matches
    // {outcome="failure"}; seed the full attempt-disposition product
    // so the matcher has a series from boot (the first failed
    // recovery after a fresh rollout is exactly the burst the alert
    // exists for). The step-down counter is seeded too — unalerted
    // today, but it pairs with the failure series on dashboards and
    // the same birth-gap reasoning applies.
    SeededSeries {
        name: "rio_scheduler_recovery_total",
        label: Some(("outcome", RECOVERY_OUTCOMES)),
    },
    SeededSeries {
        name: "rio_scheduler_recovery_step_down_total",
        label: None,
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
    // bughunt-13 F13: the establishment-defer wedge's age surface —
    // the oldest currently-deferred expired attempt's seconds past
    // its establishment window, recomputed (and zeroed) by every
    // sweep pass. Leader-owned like the open-attempt gauges it is
    // derived beside; reset value 0.0 = "no deferral wedge", so a
    // failover reads alert-neutral until the new leader's first
    // sweep republishes from ground truth.
    EstablishDeferAge => ("rio_scheduler_establish_defer_age_seconds", None, 0.0),
    // Ratio gauge: neutral is 1.0 (in-band). A 0.0 sweep would read as
    // "clamped at the low edge" and fire RioSlaPriorDivergenceClamped
    // (`<= 0.5`) on every failover — the deposed-leader wedge this
    // family exists to prevent, inverted.
    SlaPriorDivergence =>
        ("rio_scheduler_sla_prior_divergence", Some(("param", DIVERGENCE_PARAMS)), 1.0),
    // sh-018b: estimator_poller liveness. Emitted from the actor's
    // housekeeping cadence (NOT the poller loop) so it climbs when the
    // poller has panicked. Leader-only — the poller is leader-gated, so
    // a standby's `last_refresh_wall` is meaningless. Reset 0.0 =
    // alert-neutral on failover; the new leader's first poller tick
    // (tokio::interval fires immediately) re-stamps `last_refresh_wall`
    // before the first cadence emit.
    SlaRefreshAge => ("rio_scheduler_sla_refresh_age_seconds", None, 0.0),
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
    /// What a REBOUND transition (holder change observed late on a
    /// still-leading round — `sched.lease.rebound`) runs for this
    /// member. A required field, not a defaulted method: a new member
    /// cannot be silently skipped on the rebound axis — the struct
    /// literal will not compile without an explicit choice
    /// (merged_bug_212: the cost latch was Compound in spirit but the
    /// rebound delivered acquire-only, so its lose cell never ran).
    pub rebound: ReboundPolicy,
}

/// Per-member rebound disposition for [`LeaderEdge`].
pub enum ReboundPolicy {
    /// Run the lose cell, then the acquire cell — the rebound is a
    /// compressed lose→acquire pair whose standby interval was never
    /// locally observed; members whose lose cell repairs state the
    /// foreign term may have invalidated need both halves.
    Compound,
    /// Run only the acquire cell. Carries the member's written
    /// rationale — opting out of the lose half must say why it is
    /// sound (e.g. an idempotent acquire that re-derives everything).
    #[allow(dead_code)] // No member opts out today; the variant is the policy surface.
    AcquireOnly(&'static str),
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
/// CHECK-TO-WRITE DISTANCE (merged_bug_146, the round-10 axis): this
/// registry covers latch WRITERS; the distance between a leadership
/// READ and a durable WRITE is covered structurally — the SLA plane's
/// PG mutators are reachable only through `sla::cost::
/// LeaderOwnedPersist`, which re-verifies the latch AND the captured
/// tenure generation AT the write boundary, with the per-row
/// monotonicity qual as the PG-side fence (the W10-BB census in this
/// file's tests holds the population: no bare `.execute(` outside the
/// sealed row writers).
pub const LEADER_EDGES: &[LeaderEdge] = &[
    LeaderEdge {
        name: "cost-table-edge-reload-latch",
        // Verbatim the pre-table acquire effect: nudge housekeeping so
        // the false→true store + reload happen within ~0s of the lease
        // win, not ~600s (merged_bug_046: instance-type observations
        // landing in that window are merged forward by `carry_catalog`
        // — nothing is dropped or refused; the nudge keeps the reload
        // and the post-reload persist prompt).
        on_acquire: |a| a.cost_reload_notify.notify_one(),
        // THE missing lose-edge writer (bug_310): without it, an
        // A→B→A flap within one housekeeping tick leaves
        // `cost_was_leader == true`, the prelude skips the reload, and
        // the tick persists prices from the deposed tenure.
        on_lose: |a| a.cost_was_leader.store(false, Ordering::Relaxed),
        // The false-store is the fix (merged_bug_212): a foreign term
        // may have persisted its own prices, so the post-rebound
        // housekeeping tick must reload before it persists. The
        // momentary false on a still-leading replica is monotone-safe:
        // one spurious PG reload.
        rebound: ReboundPolicy::Compound,
    },
    LeaderEdge {
        name: "ice-epoch-watermark",
        // The lose half already reset the watermark; an acquire-side
        // repeat would be redundant (deposed replicas consume no Acks,
        // so nothing can advance `last_applied` while standby).
        on_acquire: |_| {},
        // bug_067: re-open the per-cell evidence-epoch gate so a
        // clock-behind successor controller lineage's GENUINE
        // marks/clears apply instead of no-op'ing until clock
        // catch-up. Covers handoff-away and the flap.
        on_lose: |a| a.ice.reset_epoch_gate(),
        // A rebound means a foreign term may have consumed evidence
        // this replica never saw — the compressed lose→acquire pair
        // must re-open the gate exactly like a real handoff.
        rebound: ReboundPolicy::Compound,
    },
    LeaderEdge {
        name: "leader-gauge-family",
        // No acquire effect: the next leader tick republishes every
        // member from ground truth (snapshot/sweep sites) — seeding
        // here would race the first tick for no benefit.
        on_acquire: |_| {},
        on_lose: |_| reset_leader_gauges(),
        // Declared reset on rebound: leader gauges momentarily drop to
        // their reset values until the next leader tick republishes
        // from ground truth — alert-neutral by design (stalled/
        // divergence alerts evaluate over windows ≫ one tick; the
        // reset values are each family member's declared neutral).
        rebound: ReboundPolicy::Compound,
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
    /// this pins the names stay meaningful and the table non-empty.
    /// The rebound axis is equally structural: `rebound` is a required
    /// field, so a member cannot merge without declaring its policy.)
    #[test]
    fn leader_edges_table_is_named_and_nonempty() {
        assert!(!LEADER_EDGES.is_empty());
        for e in LEADER_EDGES {
            assert!(!e.name.is_empty(), "LEADER_EDGES rows carry owner names");
            if let ReboundPolicy::AcquireOnly(rationale) = e.rebound {
                assert!(
                    !rationale.is_empty(),
                    "an AcquireOnly member must carry a written rationale"
                );
            }
        }
    }

    /// merged_bug_212 rider: the prose writer enumerations for the
    /// cost-table latch must reference the LEADER_EDGES registry
    /// instead of listing writers by hand — a hand list goes stale the
    /// moment the table gains a member or an edge axis, which is
    /// exactly how bug_310's missing lose writer and the rebound's
    /// acquire-only delivery survived review. include_str! so a future
    /// re-enumeration is a red suite, not a review hope.
    #[test]
    fn cost_latch_writer_docs_reference_the_edge_table() {
        const SITES: &[(&str, &str)] = &[
            ("sla/cost.rs", include_str!("sla/cost.rs")),
            ("main.rs", include_str!("main.rs")),
            ("actor/config.rs", include_str!("actor/config.rs")),
        ];
        const STALE: &[&str] = &[
            "written only here",
            "written only by",
            "only writer of the shared",
            "single edge-reload owner",
            "two-writer contract",
        ];
        for (name, src) in SITES {
            let lower = src.to_lowercase();
            for stale in STALE {
                assert!(
                    !lower.contains(stale),
                    "{name}: stale writer enumeration {stale:?} — point at \
                     observability::LEADER_EDGES instead"
                );
            }
            assert!(
                src.contains("LEADER_EDGES"),
                "{name}: the latch writer doc must reference the \
                 LEADER_EDGES registry"
            );
        }
    }

    /// W10-BB (merged_bug_146, [GEN-SET]): the check-to-write distance
    /// axis — the LEADER_EDGES census covered latch WRITERS but
    /// nothing forbade an await between a leadership read and a
    /// durable write. Now structural: every PG-mutating `.execute(`
    /// in the SLA cost plane lives inside the sealed row writers
    /// (`persist_rows` / `sweep_interrupt_samples_rows`), whose ONLY
    /// callers are `LeaderOwnedPersist` methods that re-verify
    /// leadership AT the write boundary. A gated-at-entry writer
    /// (check, await, bare execute) is the planted red.
    #[test]
    fn w10_bb_cost_plane_durable_writes_are_leader_owned() {
        /// Violations: `.execute(` outside the sealed-rows region
        /// (between the first sealed row fn and the
        /// `LeaderOwnedPersist` definition) in non-test source.
        fn violations(src: &str) -> Vec<usize> {
            let prod = src.split("mod tests").next().unwrap_or(src);
            let start = prod.find("async fn persist_rows");
            let end = prod.find("struct LeaderOwnedPersist");
            prod.lines()
                .enumerate()
                .scan(0usize, |pos, (i, line)| {
                    let line_start = *pos;
                    *pos += line.len() + 1;
                    Some((i, line_start, line))
                })
                .filter(|(_, at, line)| {
                    line.contains(".execute(")
                        && !match (start, end) {
                            (Some(s), Some(e)) => *at > s && *at < e,
                            _ => false,
                        }
                })
                .map(|(i, _, _)| i + 1)
                .collect()
        }

        let live = violations(include_str!("sla/cost.rs"));
        assert!(
            live.is_empty(),
            "durable writes outside the leader-owned sealed rows in \
             sla/cost.rs at lines {live:?} — route through LeaderOwnedPersist"
        );
        // R22′ plant: the gated-at-entry writer shape must red.
        let plant = "if leader.is_leader() {\n\
            refresh().await;\n\
            sqlx::query(\"UPDATE x\").execute(db.pool()).await;\n\
            }\n";
        assert_eq!(
            violations(plant).len(),
            1,
            "the gated-at-entry writer plant must red"
        );
        // Green twin: an execute inside the sealed region passes.
        let green = "async fn persist_rows() {\n\
            q.execute(db.pool()).await;\n\
            }\n\
            struct LeaderOwnedPersist;\n";
        assert!(violations(green).is_empty());
    }
}
