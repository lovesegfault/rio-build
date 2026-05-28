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
//!   ledger's `source_node` (the kube-authoritative spawn-ack binding
//!   persisted by the pull transaction), falling back to the
//!   controller's own in-memory `intent_id → node` binding when the
//!   ledger column is empty. Unattributable evidence is dropped — the
//!   runbook's "a cluster of NULLs is not a node signal" rule.
//! - **Cluster** = a node accumulating evidence for at least
//!   [`WEDGE_CLUSTER_MIN_DISTINCT_DRVS`] *distinct derivations* inside
//!   the [`WEDGE_CLUSTER_WINDOW_SECS`] window. One derivation expiring
//!   repeatedly is a build problem (its retries/establishments are
//!   handled by the retry fold), not a node problem — same
//!   discrimination the manual runbook query makes.
//!
//! Clustered nodes are fed to [`super::health::reap_unhealthy`] as the
//! union with the scheduler-reported `dead_nodes` (stream pools keep
//! feeding that input until 1d), so the existing `ReapReason::Dead` arm
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

/// Per-node deadline-expiry evidence with window pruning. One instance
/// lives on the NodeClaim-pool reconciler; `update` is called once per
/// healthy tick with that tick's open-attempt view.
#[derive(Default)]
pub(super) struct WedgeTracker {
    /// node → (derivation/intent id → epoch-secs the expiry was last
    /// observed). Values refresh while the expired attempt stays open;
    /// entries age out of the window after the attempt is established
    /// (or the node recovers and stops producing new evidence).
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
    // r[impl ctrl.nodeclaim.wedge-cluster]
    pub(super) fn update(
        &mut self,
        open_attempts: &[OpenAttempt],
        bound_intents: &HashMap<String, String>,
        now_secs: f64,
    ) -> Vec<String> {
        self.observe(open_attempts, bound_intents, now_secs);
        self.prune(now_secs);
        let wedged = self.wedged_nodes(now_secs);
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
        wedged
    }

    /// Fold one tick's open-attempt view into the evidence map. Only
    /// attempts past `deadline + grace` with a known deadline and a
    /// node attribution contribute.
    fn observe(
        &mut self,
        open_attempts: &[OpenAttempt],
        bound_intents: &HashMap<String, String>,
        now_secs: f64,
    ) {
        for a in open_attempts {
            if a.deadline_secs == 0 {
                // Deadline unknown to the scheduler — can't call it expired.
                continue;
            }
            if a.assigned_at_age_secs <= a.deadline_secs.saturating_add(WEDGE_DEADLINE_GRACE_SECS) {
                // Healthy (or still inside the abort-report grace).
                continue;
            }
            let node = if a.source_node.is_empty() {
                match bound_intents.get(&a.intent_id) {
                    Some(n) => n.clone(),
                    // Not node-attributable: never evidence against any node.
                    None => continue,
                }
            } else {
                a.source_node.clone()
            };
            self.evidence
                .entry(node)
                .or_default()
                .insert(a.intent_id.clone(), now_secs);
        }
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

/// Union of the scheduler-reported `dead_nodes` (stream pools, until
/// 1d) and the controller-side wedge clustering — the single
/// `dead_nodes`-shaped input `reap_unhealthy` consumes during
/// coexistence.
// r[impl ctrl.nodeclaim.wedge-cluster]
pub(super) fn dead_union(scheduler_reported: &[String], wedged: &[String]) -> Vec<String> {
    let mut set: HashSet<&str> = scheduler_reported.iter().map(String::as_str).collect();
    set.extend(wedged.iter().map(String::as_str));
    let mut out: Vec<String> = set.into_iter().map(str::to_owned).collect();
    out.sort();
    out
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
    // r[verify ctrl.nodeclaim.wedge-cluster]
    #[test]
    fn two_expired_drvs_on_one_node_mark_it_dead_equivalent() {
        let mut tracker = WedgeTracker::default();
        let wedged = tracker.update(
            &[
                expired("drv-a", "node-1", 5),
                expired("drv-b", "node-1", 5),
                expired("drv-c", "node-2", 5),
                healthy("drv-d", "node-3"),
            ],
            &HashMap::new(),
            10_000.0,
        );
        assert_eq!(wedged, vec!["node-1".to_string()]);

        // The union flows into `classify` exactly like dead_nodes: the
        // registered NodeClaim backing node-1 classifies Dead; node-2
        // (one expiry) and node-3 (healthy pulls) do not.
        let union = dead_union(&[], &wedged);
        let dead_set: HashSet<&str> = union.iter().map(String::as_str).collect();
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
    // r[verify ctrl.nodeclaim.wedge-cluster]
    #[test]
    fn single_expired_drv_or_healthy_pulls_do_not_mark() {
        let mut tracker = WedgeTracker::default();
        // Same single derivation observed expired on three consecutive ticks.
        for tick in 0u64..3 {
            let wedged = tracker.update(
                &[
                    expired("drv-a", "node-1", 5 + tick),
                    healthy("drv-b", "node-1"),
                ],
                &HashMap::new(),
                10_000.0 + (tick as f64) * 10.0,
            );
            assert!(wedged.is_empty(), "tick {tick}: {wedged:?}");
        }
    }

    /// Evidence ages out of the 30-minute window: two expiries observed
    /// far apart never coexist inside one window, so the node is not
    /// marked; once both are inside the window it is.
    // r[verify ctrl.nodeclaim.wedge-cluster]
    #[test]
    fn evidence_outside_the_window_does_not_count() {
        let mut tracker = WedgeTracker::default();
        let t0 = 10_000.0;
        assert!(
            tracker
                .update(&[expired("drv-a", "node-1", 5)], &HashMap::new(), t0)
                .is_empty()
        );
        // Second distinct expiry observed after the first aged out.
        let late = t0 + WEDGE_CLUSTER_WINDOW_SECS + 1.0;
        assert!(
            tracker
                .update(&[expired("drv-b", "node-1", 5)], &HashMap::new(), late)
                .is_empty(),
            "the drv-a evidence aged out; one in-window expiry must not mark"
        );
        // Both inside one window → marked.
        let wedged = tracker.update(
            &[expired("drv-a", "node-1", 5)],
            &HashMap::new(),
            late + 5.0,
        );
        assert_eq!(wedged, vec!["node-1".to_string()]);
    }

    /// Node attribution: the ledger's source_node wins; an empty
    /// source_node falls back to the controller's bound-intent map; an
    /// attempt with neither is never evidence against any node.
    // r[verify ctrl.nodeclaim.wedge-cluster]
    #[test]
    fn attribution_falls_back_to_bound_intents_and_skips_unknown() {
        let mut tracker = WedgeTracker::default();
        let mut no_node_a = expired("drv-a", "", 5);
        no_node_a.source_node = String::new();
        let mut no_node_b = expired("drv-b", "", 5);
        no_node_b.source_node = String::new();
        let mut no_node_c = expired("drv-c", "", 5);
        no_node_c.source_node = String::new();
        let bound: HashMap<String, String> = [
            ("drv-a".to_string(), "node-9".to_string()),
            ("drv-b".to_string(), "node-9".to_string()),
            // drv-c has no binding anywhere → dropped.
        ]
        .into();
        let wedged = tracker.update(&[no_node_a, no_node_b, no_node_c], &bound, 10_000.0);
        assert_eq!(wedged, vec!["node-9".to_string()]);
    }

    /// Attempts with an unknown deadline (0) are never evidence.
    // r[verify ctrl.nodeclaim.wedge-cluster]
    #[test]
    fn unknown_deadline_is_not_evidence() {
        let mut tracker = WedgeTracker::default();
        let mut a = expired("drv-a", "node-1", 5);
        a.deadline_secs = 0;
        let mut b = expired("drv-b", "node-1", 5);
        b.deadline_secs = 0;
        assert!(
            tracker
                .update(&[a, b], &HashMap::new(), 10_000.0)
                .is_empty()
        );
    }

    /// The union keeps the scheduler-reported names (stream pools still
    /// feed dead_nodes until 1d) alongside the wedge-clustered ones.
    // r[verify ctrl.nodeclaim.wedge-cluster]
    #[test]
    fn dead_union_keeps_both_sources() {
        let union = dead_union(
            &["node-stream".to_string(), "node-both".to_string()],
            &["node-pull".to_string(), "node-both".to_string()],
        );
        assert_eq!(
            union,
            vec![
                "node-both".to_string(),
                "node-pull".to_string(),
                "node-stream".to_string()
            ]
        );
    }
}
