//! The intent-decided candidate axis set — ONE projection consumed by
//! render, gate, and pack.
//!
//! merged_bug_124/126/249's shared mechanism was three consumers each
//! re-deriving a *subset* of the axes the pod render actually stamps:
//! the spawn gate read exclusions against a Ready-only node list (over-
//! fire), FFD packing ignored exclusions entirely (under-mint), and the
//! drift fingerprint covered placement only (silent drift on exclusion/
//! resource/deadline re-solves). [`RenderInputs`] is the complete axis
//! set, constructed once per intent; every destructive verdict that
//! depends on "where can this intent run / what did we render" goes
//! through it. A future axis added to the render but not here fails
//! the field-sensitivity contract test (`fingerprint` covers every
//! field by construction).
// r[impl ctrl.pool.intent-candidate-set]

use std::collections::BTreeMap;

use rio_proto::types::SpawnIntent;

/// A node as seen by the spawn gate: name + labels + cordon state.
/// NotReady nodes are deliberately KEPT (merged_bug_124(a)): a
/// NotReady-but-schedulable node is a provisioning/boot transient that
/// will host pods shortly — counting it spawnable prevents the gate
/// from manufacturing fleet-exhaustion out of a boot window. Cordoned
/// (`spec.unschedulable`) nodes are real non-candidates: the
/// kube-scheduler will never bind there.
#[derive(Debug, Clone)]
pub(crate) struct CandidateNode {
    pub name: String,
    pub labels: BTreeMap<String, String>,
    pub schedulable: bool,
}

/// The COMPLETE intent-decided axis set the pod render stamps.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct RenderInputs {
    /// `apply_intent_resources`' nodeSelector merge input.
    pub node_selector: BTreeMap<String, String>,
    /// §13a OR-of-ANDs placement terms (proto form; render converts).
    pub node_affinity: Vec<rio_proto::types::NodeSelectorTerm>,
    /// AD2 exclusion set, sorted (render: `NotIn` anti-affinity).
    pub excluded_nodes: Vec<String>,
    pub cores: u32,
    pub mem_bytes: u64,
    pub disk_bytes: u64,
    /// `ephemeral_deadline(intent)` — the value `activeDeadlineSeconds`
    /// is rendered from (the floor-180 form, NOT the raw proto field).
    pub deadline_secs: i64,
}

impl RenderInputs {
    pub(crate) fn from_intent(intent: &SpawnIntent) -> Self {
        let mut excluded = intent.excluded_nodes.clone();
        excluded.sort_unstable();
        excluded.dedup();
        Self {
            node_selector: intent.node_selector.clone().into_iter().collect(),
            node_affinity: intent.node_affinity.clone(),
            excluded_nodes: excluded,
            cores: intent.cores,
            mem_bytes: intent.mem_bytes,
            disk_bytes: intent.disk_bytes,
            deadline_secs: super::jobs::ephemeral_deadline(intent),
        }
    }

    /// Stable fingerprint over EVERY field. Replaces the placement-only
    /// `selector_fingerprint` (merged_bug_249): exclusion growth,
    /// resource re-solves, and deadline re-solves now drift the stamped
    /// annotation, so `reap_stale_for_intents`' drift arm replaces the
    /// Pending Job instead of letting it bind under stale inputs.
    ///
    /// Format is versioned by construction (`v2;` prefix): every legacy
    /// `selector_fingerprint` annotation differs from every v2 value,
    /// so the first post-deploy tick drift-reaps Pending Jobs exactly
    /// once (foreground delete + NameCollision-safe respawn; Running
    /// Jobs untouched — documented one-time churn).
    pub(crate) fn fingerprint(&self) -> String {
        let sel: Vec<String> = self
            .node_selector
            .iter()
            .map(|(k, v)| format!("{k}={v}"))
            .collect();
        let mut terms: Vec<String> = self
            .node_affinity
            .iter()
            .map(|t| {
                let mut kv: Vec<_> = t
                    .match_expressions
                    .iter()
                    .map(|r| format!("{}~{}={}", r.operator, r.key, r.values.join("+")))
                    .collect();
                kv.sort_unstable();
                kv.join("|")
            })
            .collect();
        terms.sort_unstable();
        format!(
            "v2;sel={};aff={};exc={};res={},{},{};dl={}",
            sel.join(","),
            terms.join(";"),
            self.excluded_nodes.join(","),
            self.cores,
            self.mem_bytes,
            self.disk_bytes,
            self.deadline_secs,
        )
    }

    /// Would the kube-scheduler consider `node` for this intent's pod,
    /// IGNORING the exclusion axis: nodeSelector ⊆ labels ∧ (affinity
    /// OR-of-ANDs admits). The gate's "pre" universe.
    ///
    /// Operator semantics: `In`/`NotIn`/`Exists`/`DoesNotExist` exact;
    /// `Gt`/`Lt`/unknown ADMIT (fail-open — the gate must never *prove*
    /// exhaustion through an operator it cannot evaluate).
    pub(crate) fn admits_ignoring_exclusion(&self, labels: &BTreeMap<String, String>) -> bool {
        if self
            .node_selector
            .iter()
            .any(|(k, v)| labels.get(k) != Some(v))
        {
            return false;
        }
        if self.node_affinity.is_empty() {
            return true;
        }
        self.node_affinity.iter().any(|term| {
            term.match_expressions.iter().all(|r| {
                let have = labels.get(&r.key);
                match r.operator.as_str() {
                    "In" => have.is_some_and(|v| r.values.contains(v)),
                    "NotIn" => have.is_none_or(|v| !r.values.contains(v)),
                    "Exists" => have.is_some(),
                    "DoesNotExist" => have.is_none(),
                    _ => true,
                }
            })
        })
    }

    /// Full admission: pre-universe ∧ name ∉ excluded.
    pub(crate) fn admits(&self, node_name: &str, labels: &BTreeMap<String, String>) -> bool {
        !self.excluded(node_name) && self.admits_ignoring_exclusion(labels)
    }

    pub(crate) fn excluded(&self, node_name: &str) -> bool {
        self.excluded_nodes.iter().any(|n| n == node_name)
    }
}

/// The ONE exclusion predicate (FFD + mint backstop + gate share it).
pub(crate) fn node_excluded(intent: &SpawnIntent, node_name: &str) -> bool {
    intent.excluded_nodes.iter().any(|n| n == node_name)
}

/// AD2 spawn-gate exhaustion over the candidate set (merged_bug_124):
/// `pre` = schedulable nodes the intent admits ignoring exclusion
/// (NotReady KEPT); `effective` = `pre` minus excluded. Exhausted iff
/// the intent carries exclusions, the pre-universe is non-empty (an
/// empty pre-universe is a provisioning transient — autoscaling may
/// mint an admitting node; mirrors `placeable()`'s empty-fleet defer),
/// and the exclusions consume it entirely. Intent affinity narrows the
/// universe (124(c)): a node the intent could never run on does not
/// veto exhaustion.
// r[impl sched.dispatch.fleet-exhaust+5]
pub(crate) fn no_eligible_source(intent: &SpawnIntent, candidates: &[CandidateNode]) -> bool {
    if intent.excluded_nodes.is_empty() {
        return false;
    }
    let ri = RenderInputs::from_intent(intent);
    let mut pre_nonempty = false;
    for c in candidates {
        if !c.schedulable || !ri.admits_ignoring_exclusion(&c.labels) {
            continue;
        }
        pre_nonempty = true;
        if ri.admits(&c.name, &c.labels) {
            // A fully-admitted (schedulable, matching, non-excluded)
            // node exists — the universe is not exhausted.
            return false;
        }
    }
    pre_nonempty
}

/// N-tick persistence for the exhaustion verdict (merged_bug_124(a)):
/// a single-tick observation (node restart, informer lag, autoscaler
/// churn) must not poison a derivation. The gate withholds the spawn
/// from tick 1 (the Job would sit unschedulable behind its own
/// anti-affinity anyway) but only REPORTS — i.e. poisons — once the
/// exhaustion has persisted `NO_ELIGIBLE_SOURCE_PERSIST_TICKS`
/// consecutive ticks (~30s at the 10s reconcile cadence).
// r[impl ctrl.pool.no-eligible-persist+2]
pub(crate) const NO_ELIGIBLE_SOURCE_PERSIST_TICKS: u32 = 3;

/// One streak-map update for one gated tick. Returns the new streak
/// and whether this tick should report. [`PoolStreaks::step_and_prune`]
/// is the only caller in the reconcile path.
pub(crate) fn exhausted_streak_step(prev: Option<u32>) -> (u32, bool) {
    let streak = prev.unwrap_or(0).saturating_add(1);
    (streak, streak >= NO_ELIGIBLE_SOURCE_PERSIST_TICKS)
}

/// Orphaned [`PoolStreaks`] entries (a pool removed from config stops
/// reconciling, so its entries are never pruned by its own tick)
/// expire after this long. Today's global wipe accidentally GC'd them;
/// the expiry replaces that accident with a law.
pub(crate) const POOL_STREAK_ORPHAN_EXPIRY_SECS: u64 = 120;

/// merged_bug_117: pool-keyed exhaustion streaks. The retired shape was
/// a pool-SHARED `HashMap<intent, u32>` whose per-pool reconcile did
/// `retain(|id| this_pools_gated.contains(id))` — pool B's tick wiped
/// pool A's streaks (a persistently exhausted intent in a multi-pool
/// config NEVER reached the persistence threshold), and an intent
/// gated in two overlapping pools double-stepped per wall-clock tick
/// (premature report after 2 ticks of per-pool observation).
///
/// The key is `(pool, intent)` and there is exactly ONE mutating
/// method: [`Self::step_and_prune`], scoped to one pool's tick — a
/// cross-pool wipe is INEXPRESSIBLE through the API.
#[derive(Debug, Default)]
pub struct PoolStreaks(std::collections::HashMap<(String, String), (u32, std::time::Instant)>);

impl PoolStreaks {
    // r[impl ctrl.pool.no-eligible-persist+2]
    /// Fold one pool's gated tick: prune THIS pool's entries that left
    /// the gated set, expire ORPHANED entries (any pool, untouched for
    /// [`POOL_STREAK_ORPHAN_EXPIRY_SECS`] — a removed pool never ticks
    /// again), step each gated intent's streak, and return the intent
    /// ids whose exhaustion persisted [`NO_ELIGIBLE_SOURCE_PERSIST_TICKS`]
    /// consecutive ticks OF THIS POOL (overlapping pools count their
    /// OWN observations; an already-reported streak keeps reporting
    /// harmlessly — duplicate reports are server-side no-ops).
    pub(crate) fn step_and_prune(
        &mut self,
        pool: &str,
        gated_ids: &std::collections::HashSet<&str>,
        now: std::time::Instant,
    ) -> Vec<String> {
        self.0.retain(|(p, id), (_, touched)| {
            if p == pool {
                gated_ids.contains(id.as_str())
            } else {
                now.saturating_duration_since(*touched).as_secs() < POOL_STREAK_ORPHAN_EXPIRY_SECS
            }
        });
        let mut report = Vec::new();
        for id in gated_ids {
            let key = (pool.to_owned(), (*id).to_owned());
            let prev = self.0.get(&key).map(|(s, _)| *s);
            let (streak, fire) = exhausted_streak_step(prev);
            self.0.insert(key, (streak, now));
            if fire {
                report.push((*id).to_owned());
            }
        }
        report.sort();
        report
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn node(name: &str, labels: &[(&str, &str)], schedulable: bool) -> CandidateNode {
        CandidateNode {
            name: name.into(),
            labels: labels
                .iter()
                .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
                .collect(),
            schedulable,
        }
    }

    fn intent(excluded: &[&str]) -> SpawnIntent {
        SpawnIntent {
            intent_id: "/nix/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-x.drv".into(),
            excluded_nodes: excluded.iter().map(|s| (*s).to_owned()).collect(),
            cores: 4,
            mem_bytes: 1 << 30,
            disk_bytes: 1 << 31,
            ..Default::default()
        }
    }

    /// merged_bug_124(a): a schedulable-but-NotReady node counts toward
    /// the pre-universe, so excluding the only Ready node is NOT
    /// exhaustion. (Pre-fix: the Ready-only spawnable set made this
    /// fire — the recorded red.)
    #[test]
    fn gate_counts_schedulable_not_ready_nodes() {
        let i = intent(&["n1"]);
        let candidates = vec![
            node("n1", &[], true),
            node("n2", &[], true), // NotReady in k8s terms — still a candidate
        ];
        assert!(!no_eligible_source(&i, &candidates));
    }

    /// Cordoned nodes are NOT candidates: with the only other node
    /// excluded, exhaustion holds.
    #[test]
    fn gate_ignores_cordoned_nodes() {
        let i = intent(&["n1"]);
        let candidates = vec![node("n1", &[], true), node("n2", &[], false)];
        assert!(no_eligible_source(&i, &candidates));
    }

    /// merged_bug_124(c): intent affinity narrows the universe — a node
    /// the intent can never run on does not veto exhaustion.
    #[test]
    fn gate_universe_narrowed_by_affinity() {
        let mut i = intent(&["n1"]);
        i.node_affinity = vec![rio_proto::types::NodeSelectorTerm {
            match_expressions: vec![rio_proto::types::NodeSelectorRequirement {
                key: "hw".into(),
                operator: "In".into(),
                values: vec!["metal".into()],
            }],
        }];
        // n1 admits (hw=metal) but is excluded; n2 never admits.
        let candidates = vec![
            node("n1", &[("hw", "metal")], true),
            node("n2", &[("hw", "cloud")], true),
        ];
        assert!(no_eligible_source(&i, &candidates));
        // A second admitting node un-exhausts.
        let candidates = vec![
            node("n1", &[("hw", "metal")], true),
            node("n3", &[("hw", "metal")], true),
        ];
        assert!(!no_eligible_source(&i, &candidates));
    }

    /// Empty pre-universe defers (provisioning transient), and an
    /// exclusion-free intent is never exhausted.
    #[test]
    fn gate_defers_on_empty_universe_and_no_exclusions() {
        let i = intent(&["n1"]);
        assert!(!no_eligible_source(&i, &[]));
        let mut sel = intent(&["n1"]);
        sel.node_selector = [("zone".to_owned(), "z1".to_owned())].into_iter().collect();
        // Only node lacks the selector label: pre-universe empty.
        assert!(!no_eligible_source(&sel, &[node("n1", &[], true)]));
        assert!(!no_eligible_source(&intent(&[]), &[node("n1", &[], true)]));
    }

    /// Persistence: ticks 1-2 withhold silently, tick 3 reports.
    #[test]
    fn gate_reports_only_after_persistence() {
        let (s1, r1) = exhausted_streak_step(None);
        let (s2, r2) = exhausted_streak_step(Some(s1));
        let (s3, r3) = exhausted_streak_step(Some(s2));
        assert_eq!((s1, r1), (1, false));
        assert_eq!((s2, r2), (2, false));
        assert_eq!((s3, r3), (3, true));
    }

    /// merged_bug_249: every render-stamped axis drifts the
    /// fingerprint (the placement-only legacy form let exclusion/
    /// resource/deadline re-solves bind stale Pending Jobs).
    #[test]
    fn fingerprint_is_sensitive_to_every_field() {
        let base = RenderInputs::from_intent(&intent(&["n1"]));
        let fp = base.fingerprint();
        let mut variants = Vec::new();
        let mut v = base.clone();
        v.node_selector.insert("k".into(), "v".into());
        variants.push(v);
        let mut v = base.clone();
        v.node_affinity = vec![rio_proto::types::NodeSelectorTerm {
            match_expressions: vec![rio_proto::types::NodeSelectorRequirement {
                key: "hw".into(),
                operator: "In".into(),
                values: vec!["m".into()],
            }],
        }];
        variants.push(v);
        let mut v = base.clone();
        v.excluded_nodes.push("n9".into());
        variants.push(v);
        let mut v = base.clone();
        v.cores += 1;
        variants.push(v);
        let mut v = base.clone();
        v.mem_bytes += 1;
        variants.push(v);
        let mut v = base.clone();
        v.disk_bytes += 1;
        variants.push(v);
        let mut v = base.clone();
        v.deadline_secs += 1;
        variants.push(v);
        for variant in variants {
            assert_ne!(variant.fingerprint(), fp, "axis change must drift");
        }
        // Determinism over input ordering.
        let mut a = intent(&["b", "a"]);
        let mut b = intent(&["a", "b"]);
        a.node_selector = [
            ("x".to_owned(), "1".to_owned()),
            ("y".to_owned(), "2".to_owned()),
        ]
        .into_iter()
        .collect();
        b.node_selector = [
            ("y".to_owned(), "2".to_owned()),
            ("x".to_owned(), "1".to_owned()),
        ]
        .into_iter()
        .collect();
        assert_eq!(
            RenderInputs::from_intent(&a).fingerprint(),
            RenderInputs::from_intent(&b).fingerprint()
        );
    }

    /// Three-way agreement: the gate's pre-universe and FFD's exclusion
    /// predicate are projections of the same admits() — an excluded
    /// node is never admitted, an admitted node is in the pre-universe.
    #[test]
    fn admit_agreement() {
        let i = intent(&["n1"]);
        let ri = RenderInputs::from_intent(&i);
        let labels = BTreeMap::new();
        assert!(ri.admits_ignoring_exclusion(&labels));
        assert!(!ri.admits("n1", &labels));
        assert!(ri.admits("n2", &labels));
        assert_eq!(node_excluded(&i, "n1"), ri.excluded("n1"));
        assert_eq!(node_excluded(&i, "n2"), ri.excluded("n2"));
    }
}

/// C2 formal-delta proptest plane 2: the candidate-set equivalence
/// laws. The keystone claim of the area-A refactor is that the gate,
/// the render, and the pack all project from ONE `RenderInputs`
/// universe — these properties pin the algebra of that universe
/// against a mirror specification, over arbitrary intents and fleets.
// r[verify ctrl.pool.intent-candidate-set]
// r[verify ctrl.pool.no-eligible-persist+2]
#[cfg(test)]
mod proptests {
    use std::collections::BTreeMap;

    use proptest::prelude::*;
    use rio_proto::types::{NodeSelectorRequirement, NodeSelectorTerm, SpawnIntent};

    use super::*;

    const KEYS: [&str; 3] = ["arch", "cap", "zone"];
    const VALS: [&str; 2] = ["x", "y"];
    const NODES: [&str; 4] = ["n0", "n1", "n2", "n3"];
    const OPS: [&str; 5] = ["In", "NotIn", "Exists", "DoesNotExist", "Gt"];

    fn arb_labels() -> impl Strategy<Value = BTreeMap<String, String>> {
        proptest::collection::btree_map(
            proptest::sample::select(&KEYS[..]).prop_map(str::to_owned),
            proptest::sample::select(&VALS[..]).prop_map(str::to_owned),
            0..=3,
        )
    }

    fn arb_nodes() -> impl Strategy<Value = Vec<CandidateNode>> {
        proptest::collection::vec(
            (
                proptest::sample::select(&NODES[..]),
                arb_labels(),
                any::<bool>(),
            )
                .prop_map(|(name, labels, schedulable)| CandidateNode {
                    name: name.to_owned(),
                    labels,
                    schedulable,
                }),
            0..=4,
        )
    }

    fn arb_term() -> impl Strategy<Value = NodeSelectorTerm> {
        proptest::collection::vec(
            (
                proptest::sample::select(&KEYS[..]),
                proptest::sample::select(&OPS[..]),
                proptest::collection::vec(
                    proptest::sample::select(&VALS[..]).prop_map(str::to_owned),
                    0..=2,
                ),
            )
                .prop_map(|(key, operator, values)| NodeSelectorRequirement {
                    key: key.to_owned(),
                    operator: operator.to_owned(),
                    values,
                }),
            0..=2,
        )
        .prop_map(|match_expressions| NodeSelectorTerm { match_expressions })
    }

    fn arb_intent() -> impl Strategy<Value = SpawnIntent> {
        (
            proptest::collection::btree_map(
                proptest::sample::select(&KEYS[..]).prop_map(str::to_owned),
                proptest::sample::select(&VALS[..]).prop_map(str::to_owned),
                0..=2,
            ),
            proptest::collection::vec(arb_term(), 0..=2),
            proptest::collection::vec(
                proptest::sample::select(&NODES[..]).prop_map(str::to_owned),
                0..=4,
            ),
            1u32..=8,
            1u64..=1u64 << 33,
            0u32..=7200,
        )
            .prop_map(
                |(node_selector, node_affinity, excluded_nodes, cores, mem_bytes, deadline)| {
                    SpawnIntent {
                        intent_id: "/nix/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-x.drv".into(),
                        node_selector: node_selector.into_iter().collect(),
                        node_affinity,
                        excluded_nodes,
                        cores,
                        mem_bytes,
                        disk_bytes: mem_bytes / 2,
                        deadline_secs: deadline,
                        ..Default::default()
                    }
                },
            )
    }

    /// Mirror spec of `admits_ignoring_exclusion`: selector ⊆ labels ∧
    /// (no terms ∨ some term's exprs all admit), with the fail-open
    /// arm for operators the gate cannot evaluate.
    fn mirror_pre(ri: &RenderInputs, labels: &BTreeMap<String, String>) -> bool {
        let sel_ok = ri
            .node_selector
            .iter()
            .all(|(k, v)| labels.get(k) == Some(v));
        let aff_ok = ri.node_affinity.is_empty()
            || ri.node_affinity.iter().any(|t| {
                t.match_expressions
                    .iter()
                    .all(|r| match r.operator.as_str() {
                        "In" => labels.get(&r.key).is_some_and(|v| r.values.contains(v)),
                        "NotIn" => labels.get(&r.key).is_none_or(|v| !r.values.contains(v)),
                        "Exists" => labels.contains_key(&r.key),
                        "DoesNotExist" => !labels.contains_key(&r.key),
                        _ => true,
                    })
            });
        sel_ok && aff_ok
    }

    proptest! {
        /// Law 1 (the single-universe conjunction): full admission is
        /// EXACTLY pre-universe membership minus the exclusion axis —
        /// the gate, the render's anti-affinity, and the pack can never
        /// disagree about a node.
        #[test]
        fn admits_is_pre_minus_exclusion(intent in arb_intent(), nodes in arb_nodes()) {
            let ri = RenderInputs::from_intent(&intent);
            for n in &nodes {
                prop_assert_eq!(
                    ri.admits(&n.name, &n.labels),
                    ri.admits_ignoring_exclusion(&n.labels) && !ri.excluded(&n.name)
                );
                prop_assert_eq!(ri.admits_ignoring_exclusion(&n.labels), mirror_pre(&ri, &n.labels));
                prop_assert_eq!(node_excluded(&intent, &n.name), ri.excluded(&n.name));
            }
        }

        /// Law 2 (exhaustion ≡ excluded-consumed-pre-universe): the AD2
        /// verdict is true iff exclusions exist, some schedulable node
        /// matches ignoring exclusion, and NO node is fully admitted —
        /// pinned against an independent fold over the same universe.
        #[test]
        fn exhaustion_matches_mirror(intent in arb_intent(), nodes in arb_nodes()) {
            let ri = RenderInputs::from_intent(&intent);
            let pre: Vec<&CandidateNode> = nodes
                .iter()
                .filter(|n| n.schedulable && ri.admits_ignoring_exclusion(&n.labels))
                .collect();
            let expected = !intent.excluded_nodes.is_empty()
                && !pre.is_empty()
                && pre.iter().all(|n| ri.excluded(&n.name));
            prop_assert_eq!(no_eligible_source(&intent, &nodes), expected);
        }

        /// Law 3 (fingerprint determinism + axis sensitivity): equal
        /// inputs render equal fingerprints; growing the exclusion set
        /// or re-solving the deadline drifts the fingerprint (the
        /// drift-reap trigger merged_bug_249 depends on). The deadline
        /// axis is sensitive in its RENDERED form: `ephemeral_deadline`
        /// floors at 180 s, so sub-floor re-solves correctly do NOT
        /// drift (this property originally asserted raw-field
        /// sensitivity and the plane falsified it — the floor is the
        /// law).
        #[test]
        fn fingerprint_deterministic_and_axis_sensitive(intent in arb_intent()) {
            let a = RenderInputs::from_intent(&intent);
            let b = RenderInputs::from_intent(&intent);
            prop_assert_eq!(a.fingerprint(), b.fingerprint());

            let mut grown = intent.clone();
            grown.excluded_nodes.push("zz-not-a-node".into());
            prop_assert_ne!(
                RenderInputs::from_intent(&grown).fingerprint(),
                a.fingerprint()
            );

            // Above the floor every re-solve drifts: 7300 exceeds both
            // the 180 floor and the strategy's 7200 cap, so the
            // rendered deadline always differs from the base's.
            let mut redl = intent.clone();
            redl.deadline_secs = 7300;
            prop_assert_ne!(
                RenderInputs::from_intent(&redl).fingerprint(),
                a.fingerprint()
            );

            // And BELOW the floor, re-solves are render-equivalent:
            // the fingerprint is the rendered tuple, not the raw wire.
            let mut sub_a = intent.clone();
            sub_a.deadline_secs = 0;
            let mut sub_b = intent.clone();
            sub_b.deadline_secs = 179;
            prop_assert_eq!(
                RenderInputs::from_intent(&sub_a).fingerprint(),
                RenderInputs::from_intent(&sub_b).fingerprint()
            );
        }

        /// Law 4 (persistence threshold): the streak fires iff it has
        /// reached NO_ELIGIBLE_SOURCE_PERSIST_TICKS, and a reset (None)
        /// always restarts at one-not-firing.
        #[test]
        fn streak_fires_only_at_threshold(prev in proptest::option::of(0u32..=10)) {
            let (streak, fire) = exhausted_streak_step(prev);
            prop_assert_eq!(streak, prev.unwrap_or(0).saturating_add(1));
            prop_assert_eq!(fire, streak >= NO_ELIGIBLE_SOURCE_PERSIST_TICKS);
            let (s1, f1) = exhausted_streak_step(None);
            prop_assert_eq!((s1, f1), (1, false));
        }
    }

    // r[verify ctrl.pool.no-eligible-persist+2]
    /// merged_bug_117 law 1 (recorded red: the retired pool-shared map's
    /// `retain(|id| gated_B.contains(id))` wiped pool A's streaks every
    /// B tick — a persistently exhausted intent in a multi-pool config
    /// NEVER reported): interleaved pool ticks each keep their own
    /// persistence count.
    #[test]
    fn cross_pool_ticks_do_not_wipe_each_other() {
        let mut s = PoolStreaks::default();
        let t = std::time::Instant::now();
        let a_gated: std::collections::HashSet<&str> = ["drv-a"].into();
        let b_gated: std::collections::HashSet<&str> = ["drv-b"].into();
        // A and B alternate; A's third own tick must report.
        assert!(s.step_and_prune("pool-a", &a_gated, t).is_empty());
        assert!(s.step_and_prune("pool-b", &b_gated, t).is_empty());
        assert!(s.step_and_prune("pool-a", &a_gated, t).is_empty());
        assert!(s.step_and_prune("pool-b", &b_gated, t).is_empty());
        assert_eq!(
            s.step_and_prune("pool-a", &a_gated, t),
            vec!["drv-a".to_string()],
            "pool A's third consecutive own observation must report"
        );
    }

    // r[verify ctrl.pool.no-eligible-persist+2]
    /// merged_bug_117 law 2 (recorded red: an intent gated in two
    /// overlapping pools double-stepped the shared entry — reported
    /// after 2 wall-clock ticks instead of each pool's own 3): each
    /// pool counts its OWN observations.
    #[test]
    fn overlapping_pools_count_their_own_observations() {
        let mut s = PoolStreaks::default();
        let t = std::time::Instant::now();
        let gated: std::collections::HashSet<&str> = ["drv-x"].into();
        // Two wall-clock rounds of both pools: 4 steps total, but no
        // single pool has seen 3 yet.
        for _ in 0..2 {
            assert!(s.step_and_prune("pool-a", &gated, t).is_empty());
            assert!(s.step_and_prune("pool-b", &gated, t).is_empty());
        }
        // Each pool's own third observation reports independently.
        assert_eq!(
            s.step_and_prune("pool-a", &gated, t),
            vec!["drv-x".to_string()]
        );
        assert_eq!(
            s.step_and_prune("pool-b", &gated, t),
            vec!["drv-x".to_string()]
        );
    }

    // r[verify ctrl.pool.no-eligible-persist+2]
    /// Orphan expiry: a pool removed from config never ticks again —
    /// its entries expire at POOL_STREAK_ORPHAN_EXPIRY_SECS instead of
    /// living forever (the retired global wipe GC'd them by accident;
    /// the law replaces the accident). Observable: A's streak restarts
    /// after the gap instead of resuming.
    #[test]
    fn orphaned_pool_entries_expire() {
        let mut s = PoolStreaks::default();
        let t0 = std::time::Instant::now();
        let gated: std::collections::HashSet<&str> = ["drv-a"].into();
        let other: std::collections::HashSet<&str> = ["drv-b"].into();
        assert!(s.step_and_prune("pool-a", &gated, t0).is_empty());
        assert!(s.step_and_prune("pool-a", &gated, t0).is_empty()); // streak 2
        // pool-a removed from config; only pool-b ticks, past the expiry.
        let late = t0 + std::time::Duration::from_secs(POOL_STREAK_ORPHAN_EXPIRY_SECS + 1);
        assert!(s.step_and_prune("pool-b", &other, late).is_empty());
        // pool-a re-added: its old streak expired, so this is tick 1, not 3.
        assert!(
            s.step_and_prune("pool-a", &gated, late).is_empty(),
            "an expired orphan streak must restart, not resume at 3"
        );
    }
}
