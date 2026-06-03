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
// r[impl ctrl.pool.no-eligible-persist]
pub(crate) const NO_ELIGIBLE_SOURCE_PERSIST_TICKS: u32 = 3;

/// One streak-map update for one gated tick. Returns the new streak
/// and whether this tick should report. Callers prune entries for
/// intents that left the set or whose universe un-exhausted.
pub(crate) fn exhausted_streak_step(prev: Option<u32>) -> (u32, bool) {
    let streak = prev.unwrap_or(0).saturating_add(1);
    (streak, streak >= NO_ELIGIBLE_SOURCE_PERSIST_TICKS)
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
