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

pub(super) use seal::Sealed;

/// One tick's wedge verdict: per-node Dead-equivalents, a systemic
/// pattern that marks nothing, or an unobserved tick.
#[derive(Debug)]
pub(super) enum WedgeVerdict {
    /// Nodes past the cluster threshold — the only permitted feed of
    /// `health::reap_unhealthy`'s Dead arm.
    NodeWedged(Vec<String>, Sealed),
    /// More than [`WEDGE_SYSTEMIC_FRACTION`] of the windowed
    /// population is past the threshold: shared cause, nothing marked.
    Systemic {
        affected: usize,
        of: usize,
        _sealed: Sealed,
    },
    /// The open-attempt view was unobserved this tick (RPC failure):
    /// no verdict — retained evidence neither marks nor suppresses,
    /// and the marked set is NOT re-derived (bug_151: previously
    /// conflated with an empty `NodeWedged`, minted OUTSIDE the
    /// sealed exit).
    Unobserved(Sealed),
}

/// Commensurable verdict populations (merged_bug_009): the numerator
/// and denominator of the systemic guard, paired by the ONLY
/// constructor so `affected ≤ of` holds BY CONSTRUCTION —
/// `wedged ⊆ evidence-nodes ⊆ population` where
/// `population = evidence-nodes ∪ this-tick fleet`. The retired guard
/// compared retained-evidence nodes against the this-tick fleet alone
/// (incommensurable: `Systemic{affected: 2, of: 1}` was
/// representable, and an empty fleet escaped the guard entirely —
/// the mass-Dead-reap polarity after an observation outage).
struct WedgePopulations {
    /// Nodes past the cluster threshold (sorted, deduplicated).
    wedged: Vec<String>,
    /// `|evidence-nodes ∪ fleet|` — every node the window knows about.
    population: usize,
}

impl WedgePopulations {
    /// The only constructor: derive both populations from the SAME
    /// window state.
    fn from_window(
        evidence: &HashMap<String, HashMap<String, f64>>,
        fleet: &HashSet<String>,
        now_secs: f64,
    ) -> Self {
        let mut wedged: Vec<String> = evidence
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
        wedged.sort();
        let population = evidence
            .keys()
            .chain(fleet.iter())
            .collect::<HashSet<_>>()
            .len();
        Self { wedged, population }
    }
}

/// Per-node deadline-expiry evidence with window pruning. One instance
/// lives on the NodeClaim-pool reconciler; `update` is called once per
/// tick with that tick's open-attempt view (or `None` when the view
/// RPC failed) and the backing nodes the controller reaped since the
/// last call.
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
    /// merged_bug_163: the episode latch. Set on every Systemic
    /// verdict; `observe` admits an expired attempt as evidence ONLY
    /// when its expiry instant is strictly newer — the episode's
    /// still-open attempts re-present every tick, and without the
    /// gate they re-anchor fresh the tick after the drain (the
    /// trailing-edge reap) while sub-threshold participants' anchors
    /// pair with any later blip.
    last_suppression_secs: Option<f64>,
}

/// The verdict seal, CONFINED (bug_151): `Sealed` and the single
/// epilogue both live in this module, so a verdict token cannot be
/// minted anywhere else in the file — an early return in `update`
/// that skips the epilogue does not compile. (The previous shape
/// relied on field privacy, which is module-scoped: `update`'s
/// None arm freely minted `Sealed(())` 110 lines below the doc that
/// called that unrepresentable.)
mod seal {
    use super::{
        WEDGE_CLUSTER_MIN_DISTINCT_DRVS, WEDGE_SYSTEMIC_FRACTION, WedgePopulations, WedgeTracker,
        WedgeVerdict,
    };

    /// Proof-of-origin token for [`WedgeVerdict`]: constructible only
    /// inside this module, whose only verdict producer is
    /// [`finalize`] — the single exit that runs the full epilogue.
    #[derive(Debug)]
    pub(in crate::reconcilers::nodeclaim_pool) struct Sealed(());

    /// The single verdict exit: every arm runs its full epilogue.
    ///
    /// - `None` (unobserved tick, bug_151): a distinct
    ///   [`WedgeVerdict::Unobserved`] — retained evidence neither
    ///   marks nor suppresses, and `marked` is NOT re-derived (an RPC
    ///   blip draining it would double-count the next transitions).
    /// - Systemic (merged_bug_163): suppression counter + warn +
    ///   WHOLE-EPISODE evidence drain (every node in the window, not
    ///   just the wedged subset — a sub-threshold participant's
    ///   episode anchor must not survive as half of a future pair) +
    ///   the suppression watermark `observe` gates admission on (the
    ///   still-open attempts of the episode re-present every tick;
    ///   their expiries predate the watermark, so they cannot
    ///   re-anchor — a genuinely stuck node re-detects from
    ///   [`WEDGE_CLUSTER_MIN_DISTINCT_DRVS`] fresh POST-episode
    ///   expiries, the §5-Q21 direction now extended to the whole
    ///   population) + marked-set re-derivation.
    /// - NodeWedged: transition-gated counter + marked-set
    ///   re-derivation.
    pub(super) fn finalize(
        t: &mut WedgeTracker,
        populations: Option<WedgePopulations>,
        now_secs: f64,
    ) -> WedgeVerdict {
        let Some(WedgePopulations { wedged, population }) = populations else {
            return WedgeVerdict::Unobserved(Sealed(()));
        };
        let affected = wedged.len();
        let systemic =
            affected >= 2 && (affected as f64 / population as f64) > WEDGE_SYSTEMIC_FRACTION;
        let survivors: Vec<String> = if systemic {
            metrics::counter!("rio_controller_wedge_systemic_suppressed_total").increment(1);
            tracing::warn!(
                affected = wedged.len(),
                of = population,
                "wedge clustering suppressed: >{WEDGE_SYSTEMIC_FRACTION} of the windowed \
                 population is past the expiry threshold — systemic cause (report-path \
                 outage, store brownout), not per-node wedges; marking nothing, draining \
                 the WHOLE episode's evidence and latching the suppression watermark \
                 (see the hung-node runbook's systemic discrimination)"
            );
            // merged_bug_163: drain the whole episode and latch it.
            t.evidence.clear();
            t.last_suppression_secs = Some(now_secs);
            Vec::new()
        } else {
            for node in &wedged {
                if t.marked.insert(node.clone()) {
                    metrics::counter!("rio_controller_node_wedge_marked_total").increment(1);
                    tracing::warn!(
                        node = %node,
                        "node marked Dead-equivalent: ≥{WEDGE_CLUSTER_MIN_DISTINCT_DRVS} distinct \
                         derivations' pull attempts expired on it inside the window (OA2 clustering)"
                    );
                }
            }
            wedged
        };
        // Shared epilogue tail: `marked` tracks exactly the
        // currently-wedged set (nodes that fell under the threshold —
        // or whose episode was drained — leave it, so a later re-wedge
        // counts as a new transition).
        t.marked.retain(|n| survivors.contains(n));
        if systemic {
            // `survivors` is empty on this arm; the verdict reports the
            // pre-drain populations.
            WedgeVerdict::Systemic {
                affected,
                of: population,
                _sealed: Sealed(()),
            }
        } else {
            WedgeVerdict::NodeWedged(survivors, Sealed(()))
        }
    }
}

impl WedgeTracker {
    /// One tick: evict reaped nodes, fold the view (if observed),
    /// prune the window, and produce the verdict through the single
    /// sealed exit.
    ///
    /// - `view = None` (the open-attempt RPC failed): the tick skips
    ///   observation AND verdict — previously accumulated evidence
    ///   stays, and no node is marked from stale data. This makes the
    ///   call-site comment literally true (the retired path ran the
    ///   full verdict over an empty fleet, which mass-marked every
    ///   retained-evidence node right after a systemic episode).
    /// - `reaped_since_last` is a REQUIRED argument: reap feedback
    ///   cannot be forgotten by a call site (the backing nodes the
    ///   controller deleted since the previous tick — their evidence
    ///   and marked entries are dead, and must not re-feed the Dead
    ///   arm or inflate the systemic populations).
    // r[impl ctrl.nodeclaim.wedge-cluster+2]
    // r[impl ctrl.nodeclaim.wedge-two-axis+3]
    pub(super) fn update(
        &mut self,
        view: Option<&[OpenAttempt]>,
        reaped_since_last: &std::collections::BTreeSet<String>,
        now_secs: f64,
    ) -> WedgeVerdict {
        for node in reaped_since_last {
            self.evidence.remove(node);
            self.marked.remove(node);
        }
        let Some(open_attempts) = view else {
            // bug_151: the unobserved tick routes through the SAME
            // sealed exit as every verdict — `seal::finalize` is the
            // only place a token can be minted, so this arm cannot
            // skip the epilogue accounting; the epilogue itself
            // branches on the unobserved case (an RPC blip must not
            // drain `marked` and double-count later transitions).
            return seal::finalize(self, None, now_secs);
        };
        let fleet = self.observe(open_attempts, now_secs);
        self.prune(now_secs);
        let populations = WedgePopulations::from_window(&self.evidence, &fleet, now_secs);
        seal::finalize(self, Some(populations), now_secs)
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
            // merged_bug_163: evidence admission must beat the
            // suppression watermark. The attempt's expiry instant is
            // recoverable from its age: it crossed deadline+grace
            // `over` seconds ago. An expiry at or before the last
            // Systemic verdict belongs to the suppressed episode —
            // the same still-open attempts must not re-anchor at the
            // trailing edge, and a sub-threshold participant's
            // episode expiry must not pair with a later blip. A
            // genuinely NEW expiry (after the watermark) admits
            // normally.
            let over = a
                .assigned_at_age_secs
                .saturating_sub(a.deadline_secs.saturating_add(WEDGE_DEADLINE_GRACE_SECS));
            let expiry_epoch = now_secs - over as f64;
            if self
                .last_suppression_secs
                .is_some_and(|w| expiry_epoch <= w)
            {
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
}

#[cfg(test)]
mod tests {
    use super::super::NodeClaimPoolConfig;
    fn no_reaps() -> std::collections::BTreeSet<String> {
        std::collections::BTreeSet::new()
    }

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
            WedgeVerdict::NodeWedged(n, _) => n,
            WedgeVerdict::Systemic { affected, of, .. } => {
                panic!("unexpected systemic verdict ({affected}/{of})")
            }
            WedgeVerdict::Unobserved(_) => panic!("unexpected unobserved verdict"),
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
    /// and `classify` consumes it as the SOLE Dead input (the removed
    /// `dead_nodes` field — reserved in the proto since the 1d sweep —
    /// fed the same arm in its day; see the module header).
    // r[verify ctrl.nodeclaim.wedge-cluster+2]
    #[test]
    fn two_expired_drvs_on_one_node_mark_it_dead_equivalent() {
        let mut tracker = WedgeTracker::default();
        let wedged = nodes(tracker.update(
            Some(&[
                expired("drv-a", "node-1", 5),
                expired("drv-b", "node-1", 5),
                expired("drv-c", "node-2", 5),
                healthy("drv-d", "node-3"),
            ]),
            &no_reaps(),
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
    // r[verify ctrl.nodeclaim.wedge-cluster+2]
    #[test]
    fn single_expired_drv_or_healthy_pulls_do_not_mark() {
        let mut tracker = WedgeTracker::default();
        // Same single derivation observed expired on three consecutive ticks.
        for tick in 0u64..3 {
            let wedged = nodes(tracker.update(
                Some(&[
                    expired("drv-a", "node-1", 5 + tick),
                    healthy("drv-b", "node-1"),
                ]),
                &no_reaps(),
                10_000.0 + (tick as f64) * 10.0,
            ));
            assert!(wedged.is_empty(), "tick {tick}: {wedged:?}");
        }
    }

    /// Evidence ages out of the 30-minute window: two expiries observed
    /// far apart never coexist inside one window, so the node is not
    /// marked; once both are inside the window it is.
    // r[verify ctrl.nodeclaim.wedge-cluster+2]
    #[test]
    fn evidence_outside_the_window_does_not_count() {
        let mut tracker = WedgeTracker::default();
        let t0 = 10_000.0;
        assert!(
            nodes(tracker.update(Some(&[expired("drv-a", "node-1", 5)]), &no_reaps(), t0))
                .is_empty()
        );
        // Second distinct expiry observed after the first aged out.
        let late = t0 + WEDGE_CLUSTER_WINDOW_SECS + 1.0;
        assert!(
            nodes(tracker.update(Some(&[expired("drv-b", "node-1", 5)]), &no_reaps(), late))
                .is_empty(),
            "the drv-a evidence aged out; one in-window expiry must not mark"
        );
        // Both inside one window → marked.
        let wedged = nodes(tracker.update(
            Some(&[expired("drv-a", "node-1", 5), expired("drv-b", "node-1", 5)]),
            &no_reaps(),
            late + 5.0,
        ));
        assert_eq!(wedged, vec!["node-1".to_string()]);
    }

    /// Attempts with an unknown deadline (0) are never evidence.
    // r[verify ctrl.nodeclaim.wedge-cluster+2]
    #[test]
    fn unknown_deadline_is_not_evidence() {
        let mut tracker = WedgeTracker::default();
        let mut a = expired("drv-a", "node-1", 5);
        a.deadline_secs = 0;
        let mut b = expired("drv-b", "node-1", 5);
        b.deadline_secs = 0;
        assert!(nodes(tracker.update(Some(&[a, b]), &no_reaps(), 10_000.0)).is_empty());
    }

    /// C2/222 leg 1: materialization attempts are store-side fetches —
    /// their deadline expiry says nothing about a *node* (the stamped
    /// source_node is the stale builder binding). They are never wedge
    /// evidence.
    // r[verify ctrl.nodeclaim.wedge-two-axis+3]
    #[test]
    fn materialization_attempts_are_never_wedge_evidence() {
        let mut tracker = WedgeTracker::default();
        let mut a = expired("drv-a", "node-1", 5);
        a.attempt_kind = rio_proto::types::AttemptKind::Materialization as i32;
        let mut b = expired("drv-b", "node-1", 5);
        b.attempt_kind = rio_proto::types::AttemptKind::Materialization as i32;
        let wedged = nodes(tracker.update(Some(&[a, b]), &no_reaps(), 10_000.0));
        assert!(
            wedged.is_empty(),
            "two expired MATERIALIZATION attempts on one node must not mark it: {wedged:?}"
        );
    }

    /// C2/222 leg 2: an attempt the ledger cannot attribute to a node
    /// (empty source_node) is never evidence — the newest-pod-wins
    /// in-memory binding attributes an old attempt's expiry to the
    /// *replacement* pod's healthy node.
    // r[verify ctrl.nodeclaim.wedge-two-axis+3]
    #[test]
    fn empty_source_node_never_attributes() {
        let mut tracker = WedgeTracker::default();
        let mut a = expired("drv-a", "", 5);
        a.source_node = String::new();
        let mut b = expired("drv-b", "", 5);
        b.source_node = String::new();
        let wedged = nodes(tracker.update(Some(&[a, b]), &no_reaps(), 10_000.0));
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
    // r[verify ctrl.nodeclaim.wedge-two-axis+3]
    #[test]
    fn evidence_window_anchors_at_first_observation() {
        let mut tracker = WedgeTracker::default();
        let t0 = 10_000.0;
        // drv-a stays expired-and-open, re-observed every 600s well past
        // the 1800s window.
        let mut t = t0;
        while t < t0 + WEDGE_CLUSTER_WINDOW_SECS + 600.0 {
            let wedged =
                nodes(tracker.update(Some(&[expired("drv-a", "node-1", 5)]), &no_reaps(), t));
            assert!(wedged.is_empty(), "single drv must never mark: {wedged:?}");
            t += 600.0;
        }
        // drv-b expires now — drv-a's FIRST observation is > window ago.
        let wedged = tracker.update(
            Some(&[expired("drv-a", "node-1", 5), expired("drv-b", "node-1", 5)]),
            &no_reaps(),
            t,
        );
        let wedged = match wedged {
            WedgeVerdict::NodeWedged(n, _) => n,
            // A 1-node attributed fleet with 1 clustered node is not
            // systemic by the >=2 affected guard; reaching here means
            // the anchor regressed.
            WedgeVerdict::Systemic { affected, of, .. } => {
                panic!("unexpected systemic verdict ({affected}/{of})")
            }
            WedgeVerdict::Unobserved(_) => panic!("unexpected unobserved verdict"),
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
    // r[verify ctrl.nodeclaim.wedge-two-axis+3]
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
        let verdict = tracker.update(Some(&view), &no_reaps(), 10_000.0);
        assert!(
            matches!(
                verdict,
                WedgeVerdict::Systemic {
                    affected: 4,
                    of: 4,
                    ..
                }
            ),
            "all-nodes-expiring is systemic; the Dead input must be empty: {verdict:?}"
        );
    }
}

/// C2 formal-delta proptest plane (bughunt-2 slot 5): the wedge
/// verdict laws over 2-5-tick TRAJECTORIES, checked against an
/// independent set-algebra oracle — NOT a mirror of the
/// implementation (the retired single-tick mirror restated the fold
/// and would have been wrong together with it). The oracle computes,
/// from the raw trajectory alone: which (node, drv) anchors are live
/// in each tick's window (first-observation anchored, eviction- and
/// drain-aware), and from that the expected verdict populations.
// r[verify ctrl.nodeclaim.wedge-two-axis+3]
#[cfg(test)]
mod proptests {
    use proptest::prelude::*;
    use rio_proto::types::OpenAttempt;
    use std::collections::{BTreeMap, BTreeSet};

    use super::*;

    const DRVS: [&str; 3] = ["d0", "d1", "d2"];
    const NODES: [&str; 4] = ["", "n0", "n1", "n2"];

    /// One trajectory tick: the observed view (None = RPC failure),
    /// plus the backing nodes reaped since the last tick.
    #[derive(Debug, Clone)]
    struct Tick {
        view: Option<Vec<OpenAttempt>>,
        reaped: BTreeSet<String>,
    }

    fn arb_attempt() -> impl Strategy<Value = OpenAttempt> {
        (
            proptest::sample::select(&DRVS[..]),
            proptest::sample::select(&NODES[..]),
            prop_oneof![
                3 => Just(rio_proto::types::AttemptKind::Build as i32),
                1 => Just(rio_proto::types::AttemptKind::Materialization as i32),
                1 => Just(0i32),
            ],
            prop_oneof![Just(0u64), Just(10u64)],
            prop_oneof![Just(0u64), Just(5u64), Just(100u64)],
        )
            .prop_map(|(intent, node, kind, deadline, age)| OpenAttempt {
                intent_id: intent.to_owned(),
                source_node: node.to_owned(),
                attempt_kind: kind,
                deadline_secs: deadline,
                assigned_at_age_secs: age,
                ..Default::default()
            })
    }

    fn arb_tick() -> impl Strategy<Value = Tick> {
        (
            prop_oneof![
                4 => proptest::collection::vec(arb_attempt(), 0..=8).prop_map(Some),
                1 => Just(None),
            ],
            proptest::collection::btree_set(
                proptest::sample::select(&NODES[1..]).prop_map(str::to_owned),
                0..=2,
            ),
        )
            .prop_map(|(view, reaped)| Tick { view, reaped })
    }

    /// Independent oracle state: live anchors per (node, drv) and the
    /// expected marked set, evolved by SET ALGEBRA over the trajectory
    /// (window arithmetic, first-anchor keep, eviction removal,
    /// drain-on-systemic), never by calling the implementation.
    #[derive(Default)]
    struct Oracle {
        anchors: BTreeMap<(String, String), f64>,
        marked: BTreeSet<String>,
        /// merged_bug_163 mirror: the suppression watermark.
        watermark: Option<f64>,
    }

    enum Expect {
        Skipped,
        PerNode(Vec<String>),
        Systemic { affected: usize, of: usize },
    }

    impl Oracle {
        fn step(&mut self, tick: &Tick, now: f64) -> Expect {
            for n in &tick.reaped {
                self.anchors.retain(|(node, _), _| node != n);
                self.marked.remove(n);
            }
            let Some(view) = &tick.view else {
                return Expect::Skipped;
            };
            let mut fleet: BTreeSet<String> = BTreeSet::new();
            for a in view {
                if a.attempt_kind != rio_proto::types::AttemptKind::Build as i32
                    || a.source_node.is_empty()
                {
                    continue;
                }
                fleet.insert(a.source_node.clone());
                if a.deadline_secs == 0
                    || a.assigned_at_age_secs <= a.deadline_secs + WEDGE_DEADLINE_GRACE_SECS
                {
                    continue;
                }
                // Watermark admission (merged_bug_163): set algebra
                // over the raw trajectory, independent of the impl.
                let over = a.assigned_at_age_secs - (a.deadline_secs + WEDGE_DEADLINE_GRACE_SECS);
                let expiry = now - over as f64;
                if self.watermark.is_some_and(|w| expiry <= w) {
                    continue;
                }
                self.anchors
                    .entry((a.source_node.clone(), a.intent_id.clone()))
                    .or_insert(now);
            }
            self.anchors
                .retain(|_, t| now - *t <= WEDGE_CLUSTER_WINDOW_SECS);
            let mut per_node: BTreeMap<&str, usize> = BTreeMap::new();
            for (node, _) in self.anchors.keys() {
                *per_node.entry(node.as_str()).or_default() += 1;
            }
            let wedged: Vec<String> = per_node
                .iter()
                .filter(|(_, c)| **c >= WEDGE_CLUSTER_MIN_DISTINCT_DRVS)
                .map(|(n, _)| (*n).to_owned())
                .collect();
            let evidence_nodes: BTreeSet<&str> =
                self.anchors.keys().map(|(n, _)| n.as_str()).collect();
            let population = evidence_nodes
                .iter()
                .copied()
                .chain(fleet.iter().map(String::as_str))
                .collect::<BTreeSet<_>>()
                .len();
            let systemic = wedged.len() >= 2
                && (wedged.len() as f64 / population as f64) > WEDGE_SYSTEMIC_FRACTION;
            if systemic {
                let affected = wedged.len();
                // Whole-episode drain + latch (merged_bug_163).
                self.anchors.clear();
                self.watermark = Some(now);
                self.marked.clear();
                Expect::Systemic {
                    affected,
                    of: population,
                }
            } else {
                self.marked = wedged.iter().cloned().collect();
                Expect::PerNode(wedged)
            }
        }
    }

    proptest! {
        /// Trajectory law: over 2-5 ticks of arbitrary views, RPC
        /// failures and reaps, the tracker's verdicts match the
        /// set-algebra oracle tick for tick — populations
        /// commensurable, drain-on-systemic, eviction honored,
        /// skip-on-None.
        #[test]
        fn trajectory_matches_set_algebra_oracle(
            ticks in proptest::collection::vec(arb_tick(), 2..=5),
        ) {
            let mut tracker = WedgeTracker::default();
            let mut oracle = Oracle::default();
            let t0 = 1_000_000.0;
            for (i, tick) in ticks.iter().enumerate() {
                let now = t0 + (i as f64) * 10.0;
                let verdict = tracker.update(tick.view.as_deref(), &tick.reaped, now);
                match (oracle.step(tick, now), verdict) {
                    (Expect::Skipped, WedgeVerdict::Unobserved(_)) => {}
                    (Expect::Skipped, v) => {
                        return Err(TestCaseError::fail(format!("tick {i}: skip produced {v:?}")));
                    }
                    (Expect::PerNode(exp), WedgeVerdict::NodeWedged(nodes, _)) => {
                        prop_assert_eq!(nodes, exp, "tick {}", i);
                    }
                    (Expect::Systemic { affected, of }, WedgeVerdict::Systemic { affected: a, of: o, .. }) => {
                        prop_assert_eq!(a, affected, "tick {}", i);
                        prop_assert_eq!(o, of, "tick {}", i);
                        prop_assert!(a <= o, "tick {i}: incommensurable {a}/{o}");
                    }
                    (Expect::PerNode(exp), v) => {
                        return Err(TestCaseError::fail(format!(
                            "tick {i}: expected per-node {exp:?}, got {v:?}"
                        )));
                    }
                    (Expect::Systemic { affected, of }, v) => {
                        return Err(TestCaseError::fail(format!(
                            "tick {i}: expected systemic {affected}/{of}, got {v:?}"
                        )));
                    }
                }
            }
        }
    }

    /// Kani substitute (rio-controller is [[bin]]-ineligible for the
    /// kani driver — recorded none-sensible-for-kani per the bug_363
    /// precedent): EXHAUSTIVE single-tick verdict check over the full
    /// 8-node bitmask universe at the real constants. Every subset of
    /// nodes carries 2 expired drvs (wedge-qualifying), every disjoint
    /// subset is healthy fleet — all 3^8 (node ∈ {qualifying, healthy,
    /// absent}) combinations, affected ≤ of asserted on every systemic
    /// verdict and the verdict matching the closed-form expectation.
    #[test]
    fn wedge_populations_exhaustive_bounded() {
        let nodes: Vec<String> = (0..8).map(|i| format!("n{i}")).collect();
        let mut checked = 0u32;
        // 3^8 assignments: 0 = absent, 1 = healthy-only, 2 = qualifying.
        for mask in 0..3u32.pow(8) {
            let mut view: Vec<OpenAttempt> = Vec::new();
            let mut expect_wedged: Vec<String> = Vec::new();
            let mut population: BTreeSet<&str> = BTreeSet::new();
            let mut m = mask;
            for n in &nodes {
                let state = m % 3;
                m /= 3;
                match state {
                    0 => {}
                    1 => {
                        view.push(OpenAttempt {
                            intent_id: "h".into(),
                            source_node: n.clone(),
                            attempt_kind: rio_proto::types::AttemptKind::Build as i32,
                            deadline_secs: 100,
                            assigned_at_age_secs: 10,
                            ..Default::default()
                        });
                        population.insert(n.as_str());
                    }
                    _ => {
                        for d in ["da", "db"] {
                            view.push(OpenAttempt {
                                intent_id: d.into(),
                                source_node: n.clone(),
                                attempt_kind: rio_proto::types::AttemptKind::Build as i32,
                                deadline_secs: 100,
                                assigned_at_age_secs: 100 + WEDGE_DEADLINE_GRACE_SECS + 5,
                                ..Default::default()
                            });
                        }
                        expect_wedged.push(n.clone());
                        population.insert(n.as_str());
                    }
                }
            }
            expect_wedged.sort();
            let systemic = expect_wedged.len() >= 2
                && (expect_wedged.len() as f64 / population.len() as f64) > WEDGE_SYSTEMIC_FRACTION;
            let mut tracker = WedgeTracker::default();
            let verdict =
                tracker.update(Some(&view), &std::collections::BTreeSet::new(), 1_000_000.0);
            match verdict {
                WedgeVerdict::Systemic { affected, of, .. } => {
                    assert!(systemic, "mask {mask}: unexpected systemic");
                    assert!(affected <= of, "mask {mask}: {affected}/{of}");
                    assert_eq!(affected, expect_wedged.len(), "mask {mask}");
                    assert_eq!(of, population.len(), "mask {mask}");
                }
                WedgeVerdict::NodeWedged(nodes, _) => {
                    assert!(!systemic, "mask {mask}: expected systemic");
                    assert_eq!(nodes, expect_wedged, "mask {mask}");
                }
                WedgeVerdict::Unobserved(_) => {
                    panic!("mask {mask}: observed view yielded Unobserved")
                }
            }
            checked += 1;
        }
        assert_eq!(checked, 3u32.pow(8), "full universe covered");
    }
}

/// merged_bug_009 + merged_bug_176 regression battery (the wave's
/// recorded reds, now pinned green).
// r[verify ctrl.nodeclaim.wedge-two-axis+3]
#[cfg(test)]
mod population_and_epilogue_tests {
    use super::*;
    fn no_reaps() -> std::collections::BTreeSet<String> {
        std::collections::BTreeSet::new()
    }

    fn expired(intent: &str, node: &str) -> OpenAttempt {
        OpenAttempt {
            intent_id: intent.into(),
            source_node: node.into(),
            assigned_at_age_secs: 100 + WEDGE_DEADLINE_GRACE_SECS + 5,
            deadline_secs: 100,
            attempt_kind: rio_proto::types::AttemptKind::Build as i32,
            ..Default::default()
        }
    }
    fn healthy(intent: &str, node: &str) -> OpenAttempt {
        OpenAttempt {
            assigned_at_age_secs: 10,
            ..expired(intent, node)
        }
    }

    /// A systemic episode followed by an observation failure must not
    /// mass-mark from retained evidence (red: tick 2 returned
    /// NodeWedged(["n0","n1","n2","n3"]) — the mass-Dead-reap polarity).
    #[test]
    fn observation_failure_after_systemic_must_not_mass_mark() {
        let mut t = WedgeTracker::default();
        let view: Vec<OpenAttempt> = (0..4)
            .flat_map(|n| {
                vec![
                    expired(&format!("d{n}x"), &format!("n{n}")),
                    expired(&format!("d{n}y"), &format!("n{n}")),
                ]
            })
            .collect();
        assert!(matches!(
            t.update(Some(&view), &no_reaps(), 1000.0),
            WedgeVerdict::Systemic { .. }
        ));
        // tick 2: ListOpenAttempts failed -> mod.rs now passes None
        // (observation AND verdict skipped).
        match t.update(None, &no_reaps(), 1010.0) {
            WedgeVerdict::Unobserved(_) => {}
            v => panic!("observation-skipped tick must yield Unobserved, got {v:?}"),
        }
    }

    /// The verdict populations are commensurable (red:
    /// `Systemic{affected: 2, of: 1}` — retained evidence vs this-tick
    /// fleet).
    #[test]
    fn affected_never_exceeds_of() {
        let mut t = WedgeTracker::default();
        let tick1: Vec<OpenAttempt> = vec![
            expired("a1", "n1"),
            expired("a2", "n1"),
            expired("b1", "n2"),
            expired("b2", "n2"),
            healthy("c", "n3"),
            healthy("d", "n4"),
            healthy("e", "n5"),
        ];
        let v1 = t.update(Some(&tick1), &no_reaps(), 1000.0);
        assert!(
            matches!(v1, WedgeVerdict::NodeWedged(..)),
            "2/5 is not systemic: {v1:?}"
        );
        // tick 2: only n3 reports; n1/n2 evidence retained in-window.
        match t.update(Some(&[healthy("c", "n3")]), &no_reaps(), 1010.0) {
            WedgeVerdict::Systemic { affected, of, .. } => assert!(
                affected <= of,
                "incommensurable systemic verdict: affected={affected} of={of}"
            ),
            WedgeVerdict::NodeWedged(..) => {}
            WedgeVerdict::Unobserved(_) => panic!("observed view yielded Unobserved"),
        }
    }

    /// Evidence for a node the controller already reaped is evicted
    /// through the REQUIRED argument (red: compile-red — no eviction
    /// input existed; behavior red: the dead node re-fed the Dead arm).
    #[test]
    fn reaped_node_evidence_is_evicted() {
        let mut t = WedgeTracker::default();
        let v = t.update(
            Some(&[expired("a1", "n1"), expired("a2", "n1"), healthy("c", "n3")]),
            &no_reaps(),
            1000.0,
        );
        assert!(matches!(v, WedgeVerdict::NodeWedged(ref n, _) if n == &vec!["n1".to_string()]));
        // The controller reaped n1's claim after that verdict; the
        // reaped set is now a REQUIRED argument (pre-fix the tracker
        // had no eviction input at all — compile-red — and retained
        // evidence re-marked the dead node every tick until the
        // window expired).
        let reaped: std::collections::BTreeSet<String> =
            std::iter::once("n1".to_string()).collect();
        match t.update(Some(&[healthy("c", "n3")]), &reaped, 1010.0) {
            WedgeVerdict::NodeWedged(nodes, _) => assert!(
                nodes.is_empty(),
                "reaped node still fed to the Dead arm from stale evidence: {nodes:?}"
            ),
            WedgeVerdict::Systemic { .. } => {}
            WedgeVerdict::Unobserved(_) => panic!("observed view yielded Unobserved"),
        }
    }

    /// A node wedged before a systemic episode counts a fresh
    /// marked-transition after it (red: marked stayed `{"n1"}` across
    /// the suppression — the early return froze the set).
    #[test]
    fn suppression_epilogue_drains_marked() {
        let mut t = WedgeTracker::default();
        let v = t.update(
            Some(&[
                expired("a1", "n1"),
                expired("a2", "n1"),
                healthy("c", "n2"),
                healthy("d", "n3"),
                healthy("e", "n4"),
            ]),
            &no_reaps(),
            1000.0,
        );
        assert!(matches!(v, WedgeVerdict::NodeWedged(ref n, _) if n == &vec!["n1".to_string()]));
        // Systemic episode.
        let view: Vec<OpenAttempt> = (0..4)
            .flat_map(|n| {
                vec![
                    expired(&format!("s{n}x"), &format!("n{n}")),
                    expired(&format!("s{n}y"), &format!("n{n}")),
                ]
            })
            .collect();
        assert!(matches!(
            t.update(Some(&view), &no_reaps(), 1010.0),
            WedgeVerdict::Systemic { .. }
        ));
        // After the episode the marked set must have been re-derived -
        // today the early return freezes it with n1 inside, so a later
        // genuine re-wedge of n1 never counts a transition. Observable
        // here: the suppressed episode left stale marked entries.
        assert!(
            t.marked.is_empty(),
            "suppression epilogue must drain the marked set: {:?}",
            t.marked
        );
    }
}

#[cfg(test)]
mod verdict_confinement_tests {
    use super::*;

    /// bug_151 (recorded red: pre-fix `update(None)` returned
    /// `NodeWedged([])` minted directly in the None arm — a verdict
    /// produced OUTSIDE the sealed exit, conflating "unobserved" with
    /// "zero wedged" while the Sealed doc called exactly that
    /// unrepresentable). The token now lives in the `seal` module
    /// whose only producer is `finalize`; the unobserved tick is its
    /// own variant.
    #[test]
    fn unobserved_tick_yields_a_distinct_verdict() {
        let mut t = WedgeTracker::default();
        let v = t.update(None, &std::collections::BTreeSet::new(), 1000.0);
        assert!(
            matches!(v, WedgeVerdict::Unobserved(..)),
            "an RPC-failed tick must produce the Unobserved verdict, got {v:?}"
        );
    }

    /// The unobserved epilogue branch: an RPC blip between two
    /// per-node verdicts must NOT drain `marked` (a drained set would
    /// double-count the next transition as a fresh edge).
    #[test]
    fn unobserved_tick_does_not_drain_marked() {
        let mut t = WedgeTracker::default();
        let view = vec![
            OpenAttempt {
                intent_id: "a1".into(),
                source_node: "n1".into(),
                assigned_at_age_secs: 100 + WEDGE_DEADLINE_GRACE_SECS + 5,
                deadline_secs: 100,
                attempt_kind: rio_proto::types::AttemptKind::Build as i32,
                ..Default::default()
            },
            OpenAttempt {
                intent_id: "a2".into(),
                source_node: "n1".into(),
                assigned_at_age_secs: 100 + WEDGE_DEADLINE_GRACE_SECS + 5,
                deadline_secs: 100,
                attempt_kind: rio_proto::types::AttemptKind::Build as i32,
                ..Default::default()
            },
        ];
        let v = t.update(Some(&view), &std::collections::BTreeSet::new(), 1000.0);
        assert!(matches!(v, WedgeVerdict::NodeWedged(ref n, _) if n == &vec!["n1".to_string()]));
        assert!(t.marked.contains("n1"));
        let v = t.update(None, &std::collections::BTreeSet::new(), 1010.0);
        assert!(matches!(v, WedgeVerdict::Unobserved(..)));
        assert!(
            t.marked.contains("n1"),
            "an RPC blip must not drain the marked set"
        );
    }
}

#[cfg(test)]
mod episode_latch_tests {
    use super::*;
    fn no_reaps() -> std::collections::BTreeSet<String> {
        std::collections::BTreeSet::new()
    }
    fn expired_at(intent: &str, node: &str, over_secs: u64) -> OpenAttempt {
        OpenAttempt {
            intent_id: intent.into(),
            source_node: node.into(),
            assigned_at_age_secs: 100 + WEDGE_DEADLINE_GRACE_SECS + over_secs,
            deadline_secs: 100,
            attempt_kind: rio_proto::types::AttemptKind::Build as i32,
            ..Default::default()
        }
    }

    /// merged_bug_163 hole 1: a sub-threshold participant of a
    /// suppressed episode (one in-window anchor — the single-pod-node
    /// case) must not keep that anchor as half of a future pair. One
    /// genuinely new post-episode expiry is "a build problem, not a
    /// node problem" (module doc) — yet pre-fix it completed the pair
    /// and Dead-reaped the node.
    #[test]
    fn suppressed_episode_subthreshold_anchor_cannot_complete_a_pair() {
        let mut t = WedgeTracker::default();
        // A and B wedge (2 distinct drvs); C participates with ONE
        // anchor. wedged={A,B}, population={A,B,C}: 2/3 > 0.5 and
        // affected >= 2 -> Systemic.
        let view = vec![
            expired_at("a1", "node-a", 5),
            expired_at("a2", "node-a", 5),
            expired_at("b1", "node-b", 5),
            expired_at("b2", "node-b", 5),
            expired_at("c1", "node-c", 5),
        ];
        assert!(matches!(
            t.update(Some(&view), &no_reaps(), 10_000.0),
            WedgeVerdict::Systemic {
                affected: 2,
                of: 3,
                ..
            }
        ));
        // Post-episode: C's old attempt closed; ONE genuinely new
        // expiry on C (fresh attempt, expired just now).
        let v = t.update(
            Some(&[expired_at("c-new", "node-c", 2)]),
            &no_reaps(),
            10_010.0,
        );
        match v {
            WedgeVerdict::NodeWedged(nodes, _) => assert!(
                nodes.is_empty(),
                "a single fresh expiry completed a pair with suppressed-episode \
                 evidence: {nodes:?}"
            ),
            other => panic!("unexpected verdict {other:?}"),
        }
    }

    /// merged_bug_163 hole 2: at the trailing edge of a healing
    /// episode, the last node whose reports land (same attempts,
    /// still open+expired) re-anchors from the SAME suppressed
    /// expiries and gets Dead-reaped — a healthy node, killed during
    /// recovery. Evidence admission must beat the suppression
    /// watermark: the same attempts' expiries predate it.
    #[test]
    fn trailing_edge_laggard_is_not_reaped() {
        let mut t = WedgeTracker::default();
        let storm: Vec<OpenAttempt> = (0..4)
            .flat_map(|n| {
                vec![
                    expired_at(&format!("d{n}x"), &format!("node-{n}"), 5),
                    expired_at(&format!("d{n}y"), &format!("node-{n}"), 5),
                ]
            })
            .collect();
        assert!(matches!(
            t.update(Some(&storm), &no_reaps(), 10_000.0),
            WedgeVerdict::Systemic { .. }
        ));
        // Heal in arbitrary order: only node-1's SAME two attempts are
        // still open+expired next tick (its reports are last in the
        // redelivery queue). Their expiries predate the suppression.
        let laggard = vec![
            expired_at("d1x", "node-1", 15),
            expired_at("d1y", "node-1", 15),
        ];
        let v = t.update(Some(&laggard), &no_reaps(), 10_010.0);
        match v {
            WedgeVerdict::NodeWedged(nodes, _) => assert!(
                nodes.is_empty(),
                "the trailing-edge laggard was re-marked from suppressed-episode \
                 expiries: {nodes:?}"
            ),
            other => panic!("unexpected verdict {other:?}"),
        }
    }
}
