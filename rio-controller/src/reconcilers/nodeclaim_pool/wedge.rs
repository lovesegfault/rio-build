//! OA2: controller-side per-node wedge clustering for pull-mode pools.
//!
//! Pull-mode executor pods do not register or heartbeat, so the
//! scheduler's heartbeat-fed hung-node detector
//! (`sched.admin.hung-node-detector`) goes blind for them: a node whose
//! kubelet/runtime wedges while the Node object stays `Ready` shows up
//! only as its builds running out their attempt deadlines one by one.
//! This module rebuilds that signal from ledger facts the controller
//! already reads each tick:
//!
//! - **Evidence** = an open pull-mode attempt whose age has exceeded its
//!   intent deadline by [`WEDGE_DEADLINE_GRACE_SECS`] (the pod was due
//!   to finish or report by then; an attempt still open past that point
//!   means nothing on the node reported anything — the establishment
//!   sweep will charge it after the report slack). Evidence is keyed on
//!   the attempt's derivation (`intent_id`) and attributed to the
//!   ledger's `source_node` ONLY (the kube-authoritative spawn-ack
//!   binding persisted by the pull transaction). Build-lane attempts
//!   only: a materialization attempt is a store-side fetch whose
//!   stamped node binding is the stale builder pod — never pod-on-node
//!   evidence. Unattributable evidence is dropped — the runbook's
//!   "a cluster of NULLs is not a node signal" rule; the in-memory
//!   newest-pod-wins intent→node fallback is gone (it attributed an
//!   old attempt's expiry to the replacement pod's healthy node).
//! - **Cluster** = a node accumulating evidence for at least
//!   [`WEDGE_CLUSTER_MIN_DISTINCT_DRVS`] *distinct derivations* inside
//!   the [`WEDGE_CLUSTER_WINDOW_SECS`] window, anchored at each
//!   derivation's FIRST observation (a stuck attempt re-observed every
//!   tick does not slide its window — expiries genuinely hours apart
//!   never cluster). One derivation expiring repeatedly is a build
//!   problem (its retries/establishments are handled by the retry
//!   fold), not a node problem — same discrimination the manual
//!   runbook query makes.
//! - **Systemic guard** = when more than
//!   [`WEDGE_SYSTEMIC_FRACTION`] of the attributed fleet is past the
//!   cluster threshold in one tick the cause is shared
//!   (scheduler/report-path outage, store brownout), not per-node
//!   wedges: the verdict is [`WedgeVerdict::Systemic`], nothing is
//!   marked, and the runbook's manual discrimination applies — the
//!   automation refuses to roll Dead-reaps across the fleet at
//!   `dead_reap_cap`.
//!
//! Clustered nodes are fed to [`super::health::reap_unhealthy`] as the
//! Dead-node input (the only such signal — the scheduler's stream-era
//! heartbeat detector and its `dead_nodes` field are gone), so the
//! existing `ReapReason::Dead` arm
//! — including its per-tick `dead_reap_cap` blast-radius bound — is the
//! single consumer. Evidence is event-shaped and in-memory only: a
//! restart under-detects for at most one window (safe direction, same
//! as the heartbeat detector), and an open-attempt RPC failure merely
//! skips one tick's observation without dropping prior evidence.

use std::collections::{HashMap, HashSet};

use rio_proto::types::OpenAttempt;

/// Sliding window over which per-node deadline-expiry evidence is
/// clustered. Mirrors the interim `RioSchedulerAttemptEstablishmentCluster`
/// alert and the manual runbook query (30 minutes).
pub(super) const WEDGE_CLUSTER_WINDOW_SECS: f64 = 1800.0;

/// Distinct derivations whose attempts must have expired on one node
/// inside the window before the node is treated as Dead-equivalent.
/// Two distinct derivations is the runbook's hung-node signature; one
/// derivation expiring repeatedly is a derivation problem.
pub(super) const WEDGE_CLUSTER_MIN_DISTINCT_DRVS: usize = 2;

/// Grace added on top of the intent deadline before an open attempt
/// counts as expired. The pod is normally killed at
/// `activeDeadlineSeconds` (≈ the deadline) and its SIGTERM-abort
/// report closes the attempt within seconds; the grace absorbs that
/// propagation so an attempt mid-abort is not evidence. Kept well under
/// the establishment report slack (default 120 s) so expired attempts
/// are still observable in the open view for several ticks before the
/// sweep removes them.
pub(super) const WEDGE_DEADLINE_GRACE_SECS: u64 = 30;

/// Fraction of the tick's attributed build fleet past the cluster
/// threshold above which the verdict is systemic (shared cause), not
/// per-node. Strictly-greater comparison; also requires at least two
/// affected nodes (a two-node fleet with one wedge is 0.5, not
/// systemic).
pub(super) const WEDGE_SYSTEMIC_FRACTION: f64 = 0.5;

// C2/077 gap 3, compile-time half: the wedge must be able to observe an
// expired attempt for its grace plus two full reconcile ticks before
// the establishment sweep may remove it from the open view. The
// scheduler validates the other half (`establishment_report_slack >=
// floor`) at config load — one shared constant, two enforced sides.
const _: () = assert!(
    WEDGE_DEADLINE_GRACE_SECS + 2 * super::TICK.as_secs()
        <= rio_common::limits::MIN_ESTABLISHMENT_REPORT_SLACK_SECS
);

/// One tick's wedge verdict: per-node Dead-equivalents, or a systemic
/// pattern that marks nothing.
#[derive(Debug)]
pub(super) enum WedgeVerdict {
    /// Nodes past the cluster threshold — the only permitted feed of
    /// `health::reap_unhealthy`'s Dead arm.
    NodeWedged(Vec<String>),
    /// More than [`WEDGE_SYSTEMIC_FRACTION`] of the attributed fleet is
    /// past the threshold: shared cause, nothing marked.
    Systemic { affected: usize, of: usize },
}

/// Per-node deadline-expiry evidence with window pruning. One instance
/// lives on the NodeClaim-pool reconciler; `update` is called once per
/// healthy tick with that tick's open-attempt view.
#[derive(Default)]
pub(super) struct WedgeTracker {
    /// node → (derivation/intent id → epoch-secs the expiry was FIRST
    /// observed — the window anchor). Entries age out of the window
    /// even while the attempt stays open (re-anchoring fresh on the
    /// next observation), so two expiries genuinely far apart never
    /// cluster.
    evidence: HashMap<String, HashMap<String, f64>>,
    /// Nodes currently past the cluster threshold — tracked so the
    /// `rio_controller_node_wedge_marked_total` counter increments once
    /// per not-wedged → wedged transition, not once per tick.
    marked: HashSet<String>,
}

impl WedgeTracker {
    /// Record this tick's expired open attempts, prune evidence that
    /// aged out of the window, and return the nodes currently past the
    /// cluster threshold (sorted, deduplicated). Increments
    /// `rio_controller_node_wedge_marked_total` for nodes newly entering
    /// the wedged set.
    // r[impl ctrl.nodeclaim.wedge-cluster+1]
    // r[impl ctrl.nodeclaim.wedge-two-axis]
    pub(super) fn update(&mut self, open_attempts: &[OpenAttempt], now_secs: f64) -> WedgeVerdict {
        let fleet = self.observe(open_attempts, now_secs);
        self.prune(now_secs);
        let wedged = self.wedged_nodes(now_secs);
        // Two-axis discrimination: most-of-fleet expiring is a shared
        // cause. Computed against THIS tick's attributed build fleet so
        // an observation outage (empty view) cannot flip a prior
        // verdict — with no fleet observed, retained evidence still
        // resolves per-node.
        if wedged.len() >= 2
            && !fleet.is_empty()
            && (wedged.len() as f64 / fleet.len() as f64) > WEDGE_SYSTEMIC_FRACTION
        {
            metrics::counter!("rio_controller_wedge_systemic_suppressed_total").increment(1);
            tracing::warn!(
                affected = wedged.len(),
                of = fleet.len(),
                "wedge clustering suppressed: >{WEDGE_SYSTEMIC_FRACTION} of the attributed \
                 fleet is past the expiry threshold — systemic cause (report-path outage, \
                 store brownout), not per-node wedges; marking nothing (see the hung-node \
                 runbook's systemic discrimination)"
            );
            return WedgeVerdict::Systemic {
                affected: wedged.len(),
                of: fleet.len(),
            };
        }
        for node in &wedged {
            if self.marked.insert(node.clone()) {
                metrics::counter!("rio_controller_node_wedge_marked_total").increment(1);
                tracing::warn!(
                    node = %node,
                    "node marked Dead-equivalent: ≥{WEDGE_CLUSTER_MIN_DISTINCT_DRVS} distinct \
                     derivations' pull attempts expired on it inside the window (OA2 clustering)"
                );
            }
        }
        // Nodes that fell back under the threshold (evidence aged out)
        // leave `marked` so a later re-wedge counts as a new transition.
        self.marked.retain(|n| wedged.contains(n));
        WedgeVerdict::NodeWedged(wedged)
    }

    /// Fold one tick's open-attempt view into the evidence map. Only
    /// BUILD attempts past `deadline + grace` with a known deadline and
    /// a ledger node attribution contribute; each (node, derivation)
    /// pair anchors at its first observation. Returns the tick's
    /// attributed build fleet (distinct source nodes across healthy AND
    /// expired attempts) — the systemic-guard denominator.
    fn observe(&mut self, open_attempts: &[OpenAttempt], now_secs: f64) -> HashSet<String> {
        let mut fleet: HashSet<String> = HashSet::new();
        for a in open_attempts {
            if a.attempt_kind != rio_proto::types::AttemptKind::Build as i32 {
                // Materialization (store fetch): the stamped node is the
                // stale builder binding — never pod-on-node evidence.
                // UNSPECIFIED (rolling-skew producer) is skipped too:
                // under-detecting for the skew window is the safe
                // direction.
                continue;
            }
            if a.source_node.is_empty() {
                // Not ledger-attributable: never evidence against any
                // node (and not fleet either — an unattributed attempt
                // says nothing about node breadth).
                continue;
            }
            fleet.insert(a.source_node.clone());
            if a.deadline_secs == 0 {
                // Deadline unknown to the scheduler — can't call it expired.
                continue;
            }
            if a.assigned_at_age_secs <= a.deadline_secs.saturating_add(WEDGE_DEADLINE_GRACE_SECS) {
                // Healthy (or still inside the abort-report grace).
                continue;
            }
            self.evidence
                .entry(a.source_node.clone())
                .or_default()
                .entry(a.intent_id.clone())
                // First-observation anchor: a stuck-open attempt
                // re-observed every tick does not slide its window. (An
                // entry that ages out while the attempt stays open
                // re-anchors fresh on the next observation — a
                // derivation stuck for a full window AND a second
                // expiry is the runbook's genuine signature.)
                .or_insert(now_secs);
        }
        fleet
    }

    /// Drop evidence older than the window and nodes left without any.
    fn prune(&mut self, now_secs: f64) {
        for per_node in self.evidence.values_mut() {
            per_node.retain(|_, t| now_secs - *t <= WEDGE_CLUSTER_WINDOW_SECS);
        }
        self.evidence.retain(|_, per_node| !per_node.is_empty());
    }

    /// Nodes whose in-window evidence spans at least
    /// [`WEDGE_CLUSTER_MIN_DISTINCT_DRVS`] distinct derivations.
    fn wedged_nodes(&self, now_secs: f64) -> Vec<String> {
        let mut out: Vec<String> = self
            .evidence
            .iter()
            .filter(|(_, per_node)| {
                per_node
                    .values()
                    .filter(|t| now_secs - **t <= WEDGE_CLUSTER_WINDOW_SECS)
                    .count()
                    >= WEDGE_CLUSTER_MIN_DISTINCT_DRVS
            })
            .map(|(node, _)| node.clone())
            .collect();
        out.sort();
        out
    }
}

#[cfg(test)]
mod tests {
    use super::super::NodeClaimPoolConfig;
    use super::super::ffd::tests::with_conds;
    use super::super::health::{ReapReason, classify};
    use super::super::sketch::{CapacityType, CellSketches};
    use super::*;

    /// One expired open attempt for `intent` on `node`, `over_secs`
    /// past its deadline+grace.
    fn expired(intent: &str, node: &str, over_secs: u64) -> OpenAttempt {
        OpenAttempt {
            intent_id: intent.into(),
            derivation: format!("/nix/store/{intent}.drv"),
            exec_id: format!("exec-{intent}"),
            executor_id: intent.into(),
            source_node: node.into(),
            generation: 1,
            assigned_at_age_secs: 100 + WEDGE_DEADLINE_GRACE_SECS + over_secs,
            deadline_secs: 100,
            // The wedge consumes BUILD evidence only (C2/222);
            // UNSPECIFIED (skew) and MATERIALIZATION are skipped.
            attempt_kind: rio_proto::types::AttemptKind::Build as i32,
        }
    }

    /// Unwrap the per-node verdict (panics on a systemic verdict —
    /// tests that expect suppression match on it directly).
    fn nodes(v: WedgeVerdict) -> Vec<String> {
        match v {
            WedgeVerdict::NodeWedged(n) => n,
            WedgeVerdict::Systemic { affected, of } => {
                panic!("unexpected systemic verdict ({affected}/{of})")
            }
        }
    }

    /// A healthy open attempt (well inside its deadline).
    fn healthy(intent: &str, node: &str) -> OpenAttempt {
        OpenAttempt {
            assigned_at_age_secs: 10,
            ..expired(intent, node, 0)
        }
    }

    /// Two attempts for distinct derivations expired on the same node
    /// inside the window → that node (and only it) is Dead-equivalent,
    /// and `classify` consumes the union exactly as it consumes
    /// scheduler-reported `dead_nodes` today.
    // r[verify ctrl.nodeclaim.wedge-cluster+1]
    #[test]
    fn two_expired_drvs_on_one_node_mark_it_dead_equivalent() {
        let mut tracker = WedgeTracker::default();
        let wedged = nodes(tracker.update(
            &[
                expired("drv-a", "node-1", 5),
                expired("drv-b", "node-1", 5),
                expired("drv-c", "node-2", 5),
                healthy("drv-d", "node-3"),
            ],
            10_000.0,
        ));
        assert_eq!(wedged, vec!["node-1".to_string()]);

        // The wedge list flows into `classify` as the Dead input: the
        // registered NodeClaim backing node-1 classifies Dead; node-2
        // (one expiry) and node-3 (healthy pulls) do not.
        let dead_set: HashSet<&str> = wedged.iter().map(String::as_str).collect();
        let cfg = NodeClaimPoolConfig::default();
        let sk = CellSketches::default();
        let live: Vec<_> = ["node-1", "node-2", "node-3"]
            .iter()
            .map(|n| {
                // ffd::tests::node names the NodeClaim `nc` and its backing
                // Node `node-{nc}`; strip the prefix to line them up.
                with_conds(
                    super::super::ffd::tests::node(
                        n.trim_start_matches("node-"),
                        "h",
                        CapacityType::Spot,
                        8,
                        0,
                        0,
                    ),
                    &[("Registered", "True", 1042.0)],
                )
            })
            .collect();
        let reaped = classify(&live, &dead_set, &sk, &cfg, 1100.0);
        assert_eq!(
            reaped.len(),
            1,
            "exactly one claim classifies Dead: {reaped:?}"
        );
        assert_eq!(reaped[0].1, ReapReason::Dead);
        assert_eq!(live[reaped[0].0].node_name.as_deref(), Some("node-1"));
    }

    /// One derivation expiring (even repeatedly observed) never marks a
    /// node, and healthy pulls contribute nothing.
    // r[verify ctrl.nodeclaim.wedge-cluster+1]
    #[test]
    fn single_expired_drv_or_healthy_pulls_do_not_mark() {
        let mut tracker = WedgeTracker::default();
        // Same single derivation observed expired on three consecutive ticks.
        for tick in 0u64..3 {
            let wedged = nodes(tracker.update(
                &[
                    expired("drv-a", "node-1", 5 + tick),
                    healthy("drv-b", "node-1"),
                ],
                10_000.0 + (tick as f64) * 10.0,
            ));
            assert!(wedged.is_empty(), "tick {tick}: {wedged:?}");
        }
    }

    /// Evidence ages out of the 30-minute window: two expiries observed
    /// far apart never coexist inside one window, so the node is not
    /// marked; once both are inside the window it is.
    // r[verify ctrl.nodeclaim.wedge-cluster+1]
    #[test]
    fn evidence_outside_the_window_does_not_count() {
        let mut tracker = WedgeTracker::default();
        let t0 = 10_000.0;
        assert!(nodes(tracker.update(&[expired("drv-a", "node-1", 5)], t0)).is_empty());
        // Second distinct expiry observed after the first aged out.
        let late = t0 + WEDGE_CLUSTER_WINDOW_SECS + 1.0;
        assert!(
            nodes(tracker.update(&[expired("drv-b", "node-1", 5)], late)).is_empty(),
            "the drv-a evidence aged out; one in-window expiry must not mark"
        );
        // Both inside one window → marked.
        let wedged = nodes(tracker.update(
            &[expired("drv-a", "node-1", 5), expired("drv-b", "node-1", 5)],
            late + 5.0,
        ));
        assert_eq!(wedged, vec!["node-1".to_string()]);
    }

    /// Attempts with an unknown deadline (0) are never evidence.
    // r[verify ctrl.nodeclaim.wedge-cluster+1]
    #[test]
    fn unknown_deadline_is_not_evidence() {
        let mut tracker = WedgeTracker::default();
        let mut a = expired("drv-a", "node-1", 5);
        a.deadline_secs = 0;
        let mut b = expired("drv-b", "node-1", 5);
        b.deadline_secs = 0;
        assert!(nodes(tracker.update(&[a, b], 10_000.0)).is_empty());
    }

    /// C2/222 leg 1: materialization attempts are store-side fetches —
    /// their deadline expiry says nothing about a *node* (the stamped
    /// source_node is the stale builder binding). They are never wedge
    /// evidence.
    // r[verify ctrl.nodeclaim.wedge-two-axis]
    #[test]
    fn materialization_attempts_are_never_wedge_evidence() {
        let mut tracker = WedgeTracker::default();
        let mut a = expired("drv-a", "node-1", 5);
        a.attempt_kind = rio_proto::types::AttemptKind::Materialization as i32;
        let mut b = expired("drv-b", "node-1", 5);
        b.attempt_kind = rio_proto::types::AttemptKind::Materialization as i32;
        let wedged = nodes(tracker.update(&[a, b], 10_000.0));
        assert!(
            wedged.is_empty(),
            "two expired MATERIALIZATION attempts on one node must not mark it: {wedged:?}"
        );
    }

    /// C2/222 leg 2: an attempt the ledger cannot attribute to a node
    /// (empty source_node) is never evidence — the newest-pod-wins
    /// in-memory binding attributes an old attempt's expiry to the
    /// *replacement* pod's healthy node.
    // r[verify ctrl.nodeclaim.wedge-two-axis]
    #[test]
    fn empty_source_node_never_attributes() {
        let mut tracker = WedgeTracker::default();
        let mut a = expired("drv-a", "", 5);
        a.source_node = String::new();
        let mut b = expired("drv-b", "", 5);
        b.source_node = String::new();
        let wedged = nodes(tracker.update(&[a, b], 10_000.0));
        assert!(
            wedged.is_empty(),
            "ledger-unattributable expiries must never mark any node: {wedged:?}"
        );
    }

    /// C2/077 gap 2: the cluster window anchors at FIRST observation.
    /// One stuck derivation re-observed every tick for over a window,
    /// plus a second derivation expiring at the very end, must not
    /// mark: the two expiries are hours apart even though both were
    /// "recently observed".
    // r[verify ctrl.nodeclaim.wedge-two-axis]
    #[test]
    fn evidence_window_anchors_at_first_observation() {
        let mut tracker = WedgeTracker::default();
        let t0 = 10_000.0;
        // drv-a stays expired-and-open, re-observed every 600s well past
        // the 1800s window.
        let mut t = t0;
        while t < t0 + WEDGE_CLUSTER_WINDOW_SECS + 600.0 {
            let wedged = nodes(tracker.update(&[expired("drv-a", "node-1", 5)], t));
            assert!(wedged.is_empty(), "single drv must never mark: {wedged:?}");
            t += 600.0;
        }
        // drv-b expires now — drv-a's FIRST observation is > window ago.
        let wedged = tracker.update(
            &[expired("drv-a", "node-1", 5), expired("drv-b", "node-1", 5)],
            t,
        );
        let wedged = match wedged {
            WedgeVerdict::NodeWedged(n) => n,
            // A 1-node attributed fleet with 1 clustered node is not
            // systemic by the >=2 affected guard; reaching here means
            // the anchor regressed.
            WedgeVerdict::Systemic { affected, of } => {
                panic!("unexpected systemic verdict ({affected}/{of})")
            }
        };
        assert!(
            wedged.is_empty(),
            "expiries first observed > window apart must not cluster: {wedged:?}"
        );
    }

    /// C2/077 gap 1: when MOST attributed nodes are accumulating
    /// expiries the cause is systemic (scheduler/report-path outage,
    /// store brownout), not per-node wedges — marking nothing beats
    /// rolling Dead-reaps across the fleet at dead_reap_cap.
    // r[verify ctrl.nodeclaim.wedge-two-axis]
    #[test]
    fn fleet_wide_expiry_is_systemic_and_marks_nothing() {
        let mut tracker = WedgeTracker::default();
        let view: Vec<OpenAttempt> = (0..4)
            .flat_map(|n| {
                vec![
                    expired(&format!("drv-{n}-x"), &format!("node-{n}"), 5),
                    expired(&format!("drv-{n}-y"), &format!("node-{n}"), 5),
                ]
            })
            .collect();
        let verdict = tracker.update(&view, 10_000.0);
        assert!(
            matches!(verdict, WedgeVerdict::Systemic { affected: 4, of: 4 }),
            "all-nodes-expiring is systemic; the Dead input must be empty: {verdict:?}"
        );
    }
}
