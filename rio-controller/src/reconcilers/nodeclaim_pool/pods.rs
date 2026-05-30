//! Per-tick Pod snapshot: `requested` sums + bound-intent index from
//! one label-selected Pod LIST.
//!
//! Replaces the watch-fed `PodRequestedCache` (controller formal
//! campaign §4(a)1 — the recomputable-cache cleanup). The reconciler
//! LISTs `rio.build/pool`-labelled pods once per tick and derives both
//! consumers' views from that single snapshot:
//!
//! - [`PodSnapshot::requested_for`] → `LiveNode.requested` (FFD's
//!   `free()` term),
//! - [`PodSnapshot::bound_intents`] → FFD's already-bound
//!   short-circuit, the OA2 wedge-clustering attribution fallback, and
//!   [`PodSnapshot::bound_intent_protos`] → the `bound_intents` set
//!   shipped to the scheduler by `report_unfulfillable`.
//!
//! A fresh LIST is *more* consistent than the watch cache it replaces:
//! the watch's lag and gap artifacts (late MODIFIED overwrites,
//! relist ghosts) were themselves a bug source — the deleted cache
//! carried three guard fixes (`mb_034` pod-name-guarded delete,
//! `bug_023` terminating-apply guard, `prune_absent` relist pruning)
//! against states a per-tick LIST cannot represent.
//!
//! # Derivation rules
//!
//! The guards' *semantics* encode a world fact that survives the
//! recompute — during a retry handoff the old pod (deletionTimestamp
//! set, phase still Running for its grace period) and the retry's new
//! pod coexist in the same LIST snapshot carrying the same
//! `rio.build/intent-id` annotation. The rules transfer 1:1:
//!
//! - **(a)** `requested[node]` sums every pod with `spec.nodeName`
//!   whose phase is not Succeeded/Failed, INCLUDING pods with
//!   `deletionTimestamp` set — a terminating pod holds node resources
//!   until it is gone.
//! - **(b)** the bound-intent index maps `intent_id → node` only from
//!   pods WITHOUT `deletionTimestamp` (and not Succeeded/Failed) — a
//!   terminating pod must not claim the binding its retry's new pod
//!   may already hold.
//! - **(c)** if more than one eligible pod carries the same
//!   `intent_id`, the newest `creationTimestamp` wins (pod name as
//!   deterministic tie-break); an intent whose only pod is terminating
//!   gets no binding entry — fail-safe: FFD falls through to
//!   fit-check, and an unbound executor is never wedge-attributed.

use std::collections::HashMap;

use k8s_openapi::api::core::v1::Pod;

use super::ffd::{parse_bytes, parse_cpu_millis};
use crate::reconcilers::pool::jobs::INTENT_ID_ANNOTATION;

/// One tick's view of builder/fetcher pods, derived by
/// [`PodSnapshot::derive`] from a single label-selected Pod LIST.
#[derive(Debug, Default)]
pub(super) struct PodSnapshot {
    /// Rule (a): node name → Σ `(cores, mem_bytes, disk_bytes)`
    /// requested by non-terminal pods bound to it.
    requested: HashMap<String, (u32, u64, u64)>,
    /// Rules (b)/(c): `intent_id → node_name` for bound, non-terminating
    /// pods.
    bound: HashMap<String, String>,
}

impl PodSnapshot {
    /// Derive the snapshot from one Pod LIST (rules (a)–(c) above).
    pub(super) fn derive(pods: &[Pod]) -> Self {
        let mut requested: HashMap<String, (u32, u64, u64)> = HashMap::new();
        // Rule (c) working state: intent_id → (creation epoch, pod name,
        // node name) of the winning pod so far.
        let mut bound: HashMap<String, (i64, String, String)> = HashMap::new();
        for pod in pods {
            // Terminal pods hold no resources and carry no binding —
            // kube-scheduler's NodeResourcesFit excludes them too; with
            // `JOB_TTL_SECS=600s` they would otherwise inflate
            // `requested` for ~60 FFD ticks per build completion.
            if matches!(
                pod.status.as_ref().and_then(|s| s.phase.as_deref()),
                Some("Succeeded" | "Failed")
            ) {
                continue;
            }
            // Pending pods (no `spec.nodeName`) reserve nothing yet.
            let Some(node) = pod.spec.as_ref().and_then(|s| s.node_name.as_deref()) else {
                continue;
            };
            let Some(name) = pod.metadata.name.as_deref() else {
                continue;
            };
            // Rule (a): terminating pods still hold node resources.
            let (c, m, d) = pod_requests(pod);
            let e = requested.entry(node.to_string()).or_insert((0, 0, 0));
            *e = (e.0 + c, e.1 + m, e.2 + d);
            // Rule (b): no binding from a terminating pod.
            if pod.metadata.deletion_timestamp.is_some() {
                continue;
            }
            let Some(id) = pod
                .metadata
                .annotations
                .as_ref()
                .and_then(|a| a.get(INTENT_ID_ANNOTATION))
            else {
                continue;
            };
            // Rule (c): newest creationTimestamp wins; pod name breaks
            // exact ties deterministically.
            let created = pod
                .metadata
                .creation_timestamp
                .as_ref()
                .map_or(0, |t| t.0.as_second());
            match bound.entry(id.clone()) {
                std::collections::hash_map::Entry::Occupied(mut o) => {
                    let (cur_created, cur_name, _) = o.get();
                    if (created, name) > (*cur_created, cur_name.as_str()) {
                        *o.get_mut() = (created, name.to_string(), node.to_string());
                    }
                }
                std::collections::hash_map::Entry::Vacant(v) => {
                    v.insert((created, name.to_string(), node.to_string()));
                }
            }
        }
        Self {
            requested,
            bound: bound
                .into_iter()
                .map(|(id, (_, _, node))| (id, node))
                .collect(),
        }
    }

    /// Rule (a) reader: `Σ (cores, mem, disk)` over non-terminal pods
    /// on `node`. `(0, 0, 0)` for a node with no pods in the snapshot.
    pub(super) fn requested_for(&self, node: &str) -> (u32, u64, u64) {
        self.requested.get(node).copied().unwrap_or((0, 0, 0))
    }

    /// Rules (b)/(c) reader: `intent_id → node_name` for bound pods.
    /// FFD short-circuits these intents to `placeable` instead of
    /// fit-checking them (their own pod's `(c,m,d)` is already in
    /// [`Self::requested_for`] — a fit-check would double-count and
    /// evict them); the OA2 wedge clustering uses the same map as its
    /// attribution fallback.
    pub(super) fn bound_intents(&self) -> &HashMap<String, String> {
        &self.bound
    }

    /// The `bound_intents` set shipped to the scheduler via
    /// `report_unfulfillable` — kube-authoritative `intent_id →
    /// spec.nodeName`, full set every tick, cardinality O(active
    /// builds).
    pub(super) fn bound_intent_protos(&self) -> Vec<rio_proto::types::BoundIntent> {
        self.bound
            .iter()
            .map(|(intent_id, node_name)| rio_proto::types::BoundIntent {
                intent_id: intent_id.clone(),
                node_name: node_name.clone(),
            })
            .collect()
    }
}

/// `Σ containers[].resources.requests` as `(cores, mem, disk)`. Whole
/// cores (truncated millicores) so the unit matches `SpawnIntent.cores`
/// and `LiveNode.allocatable.0`. Missing keys → 0.
fn pod_requests(pod: &Pod) -> (u32, u64, u64) {
    let Some(spec) = pod.spec.as_ref() else {
        return (0, 0, 0);
    };
    spec.containers.iter().fold((0, 0, 0), |(c, m, d), ctr| {
        let q = |k: &str| {
            ctr.resources
                .as_ref()
                .and_then(|r| r.requests.as_ref())
                .and_then(|r| r.get(k))
                .map(|q| q.0.as_str())
        };
        (
            c + q("cpu").map_or(0, |s| (parse_cpu_millis(s) / 1000) as u32),
            m + q("memory").map_or(0, parse_bytes),
            d + q("ephemeral-storage").map_or(0, parse_bytes),
        )
    })
}

#[cfg(test)]
mod tests {
    use k8s_openapi::apimachinery::pkg::apis::meta::v1::Time;
    use k8s_openapi::jiff::Timestamp;

    use super::*;

    const GI: u64 = 1 << 30;

    fn pod_req(name: &str, node: Option<&str>, cpu: &str, mem: &str, disk: &str) -> Pod {
        use k8s_openapi::api::core::v1::{Container, PodSpec, ResourceRequirements};
        use k8s_openapi::apimachinery::pkg::api::resource::Quantity;
        let mut p = Pod::default();
        p.metadata.name = Some(name.into());
        p.metadata.namespace = Some("rio".into());
        p.spec = Some(PodSpec {
            node_name: node.map(str::to_owned),
            containers: vec![Container {
                name: "c".into(),
                resources: Some(ResourceRequirements {
                    requests: Some(
                        [
                            ("cpu".into(), Quantity(cpu.into())),
                            ("memory".into(), Quantity(mem.into())),
                            ("ephemeral-storage".into(), Quantity(disk.into())),
                        ]
                        .into(),
                    ),
                    ..Default::default()
                }),
                ..Default::default()
            }],
            ..Default::default()
        });
        p
    }

    fn with_intent(mut p: Pod, id: &str) -> Pod {
        p.metadata
            .annotations
            .get_or_insert_default()
            .insert(INTENT_ID_ANNOTATION.into(), id.into());
        p
    }

    fn with_phase(mut p: Pod, phase: &str) -> Pod {
        p.status = Some(k8s_openapi::api::core::v1::PodStatus {
            phase: Some(phase.into()),
            ..Default::default()
        });
        p
    }

    fn with_created(mut p: Pod, epoch: i64) -> Pod {
        p.metadata.creation_timestamp = Some(Time(Timestamp::from_second(epoch).unwrap()));
        p
    }

    fn terminating(mut p: Pod) -> Pod {
        p.metadata.deletion_timestamp = Some(Time(Timestamp::now()));
        p
    }

    /// Rule (a): sums per `spec.nodeName`, truncated millicores,
    /// Pending pods (no node) contribute nothing, unknown node reads
    /// `(0,0,0)`. Port of the cache's `pod_requested_cache_sums_by_node`.
    #[test]
    fn snapshot_sums_requests_by_node() {
        let s = PodSnapshot::derive(&[
            pod_req("a", Some("n1"), "4", "8Gi", "10Gi"),
            pod_req("b", Some("n1"), "2", "4Gi", "5Gi"),
            pod_req("c", Some("n2"), "1500m", "2Gi", "1Gi"),
            // Pending pod (no nodeName) → not counted anywhere.
            pod_req("p", None, "8", "1Gi", "1Gi"),
        ]);
        assert_eq!(s.requested_for("n1"), (6, 12 * GI, 15 * GI));
        // 1500m → 1 whole core (truncated, matching SpawnIntent.cores).
        assert_eq!(s.requested_for("n2"), (1, 2 * GI, GI));
        assert_eq!(s.requested_for("unknown"), (0, 0, 0));
    }

    /// Rule (a) exclusion: Succeeded/Failed pods contribute neither
    /// requests nor bindings (kube-scheduler's NodeResourcesFit excludes
    /// them; counting them inflates `requested` for ~60 FFD ticks per
    /// completion). Port of `pod_requested_excludes_terminal_phases` +
    /// the terminal half of `pod_requested_indexes_bound_intent`.
    #[test]
    fn snapshot_excludes_terminal_phases() {
        let s = PodSnapshot::derive(&[
            with_phase(
                with_intent(pod_req("done", Some("n1"), "4", "8Gi", "10Gi"), "X"),
                "Succeeded",
            ),
            with_phase(
                with_intent(pod_req("oom", Some("n1"), "2", "4Gi", "5Gi"), "Y"),
                "Failed",
            ),
            with_phase(pod_req("run", Some("n1"), "2", "4Gi", "5Gi"), "Running"),
        ]);
        assert_eq!(
            s.requested_for("n1"),
            (2, 4 * GI, 5 * GI),
            "only the Running pod counts"
        );
        assert!(
            s.bound_intents().is_empty(),
            "terminal pods carry no binding"
        );
    }

    /// Rule (b): a bound pod's `intent_id → node` is indexed so FFD can
    /// short-circuit it to `placeable` (bug_069). Port of
    /// `pod_requested_indexes_bound_intent`.
    #[test]
    fn snapshot_indexes_bound_intent() {
        let s = PodSnapshot::derive(&[with_intent(
            pod_req("rb-x", Some("n1"), "4", "8Gi", "10Gi"),
            "abc",
        )]);
        assert_eq!(s.bound_intents().get("abc"), Some(&"n1".to_string()));
        assert_eq!(s.bound_intent_protos().len(), 1);
    }

    /// §4(a)1 gate test (design-named): the two-pods-one-intent
    /// retry-handoff overlap window. One LIST holds the old pod
    /// (deletionTimestamp set, phase still Running for the grace
    /// period, node n1) AND the retry's new pod (node n2), both
    /// carrying intent X:
    ///
    /// - the binding targets the NEW pod's node (rules (b)/(c)),
    /// - `requested` sums include BOTH pods (rule (a) — the
    ///   terminating pod holds n1's resources until gone),
    /// - the `BoundIntent` set shipped to the scheduler
    ///   (`report_unfulfillable` → wedge/hung-node attribution) targets
    ///   the non-terminating pod's node.
    ///
    /// Replaces the watch-cache guards' tests
    /// `apply_terminating_preserves_newer_binding` and
    /// `delete_preserves_newer_binding_for_same_intent`. The rest of
    /// the consequence chain (`bound_intents` → OA2 wedge attribution →
    /// Dead-arm reap) is covered by `wedge.rs`'s
    /// `attribution_falls_back_to_bound_intents_and_skips_unknown` and
    /// `health.rs`'s `dead_nodes_reaped_with_cap`.
    #[test]
    fn two_pods_one_intent_overlap_window() {
        let old = terminating(with_created(
            with_intent(pod_req("pod-a", Some("n1"), "4", "8Gi", "10Gi"), "X"),
            1000,
        ));
        let new = with_created(
            with_intent(pod_req("pod-b", Some("n2"), "4", "8Gi", "10Gi"), "X"),
            2000,
        );
        let s = PodSnapshot::derive(&[old, new]);

        // Binding targets the new pod's node.
        assert_eq!(
            s.bound_intents().get("X"),
            Some(&"n2".to_string()),
            "terminating pod-a must not hold X's binding over pod-b"
        );
        // Requested sums include both pods.
        assert_eq!(
            s.requested_for("n1"),
            (4, 8 * GI, 10 * GI),
            "terminating pod-a still holds n1's resources"
        );
        assert_eq!(s.requested_for("n2"), (4, 8 * GI, 10 * GI));
        // The scheduler-shipped BoundIntent set targets n2.
        let protos = s.bound_intent_protos();
        assert_eq!(protos.len(), 1);
        assert_eq!(protos[0].intent_id, "X");
        assert_eq!(protos[0].node_name, "n2");
    }

    /// §4(a)1 gate test (design-named): pods absent from the LIST
    /// contribute nothing — no requested sums, no binding. Successor of
    /// the watch cache's
    /// `prune_absent_evicts_stale_bound_intent_with_surviving_sibling`
    /// (there is no relist gap to prune: the LIST is the entire input).
    #[test]
    fn absent_pods_contribute_nothing() {
        // pod-a (intent X, node n1) is NOT in the LIST; only its
        // sibling pod-b (intent Y, same node) is.
        let s = PodSnapshot::derive(&[with_intent(
            pod_req("pod-b", Some("n1"), "2", "4Gi", "5Gi"),
            "Y",
        )]);
        assert_eq!(s.bound_intents().get("X"), None, "absent pod → no binding");
        assert_eq!(
            s.bound_intents().get("Y"),
            Some(&"n1".to_string()),
            "surviving sibling keeps its own binding"
        );
        assert_eq!(
            s.requested_for("n1"),
            (2, 4 * GI, 5 * GI),
            "only the listed pod's requests count"
        );
    }

    /// Rule (c) fail-safe half: an intent whose ONLY pod is terminating
    /// gets no binding entry — FFD falls through to fit-check rather
    /// than short-circuiting onto a node whose pod is on its way out.
    #[test]
    fn intent_with_only_terminating_pod_is_unbound() {
        let s = PodSnapshot::derive(&[terminating(with_intent(
            pod_req("pod-a", Some("n1"), "4", "8Gi", "10Gi"),
            "X",
        ))]);
        assert_eq!(s.bound_intents().get("X"), None);
        assert!(s.bound_intent_protos().is_empty());
        // Rule (a) still applies: the resources are held.
        assert_eq!(s.requested_for("n1"), (4, 8 * GI, 10 * GI));
    }

    /// Rule (c) conflict half: two non-terminating pods carrying the
    /// same intent (a LIST racing a re-spawn) → the newest
    /// creationTimestamp wins; an exact timestamp tie breaks
    /// deterministically by pod name.
    #[test]
    fn newest_pod_wins_binding_conflict() {
        let s = PodSnapshot::derive(&[
            with_created(
                with_intent(pod_req("pod-old", Some("n1"), "4", "8Gi", "10Gi"), "X"),
                1000,
            ),
            with_created(
                with_intent(pod_req("pod-new", Some("n2"), "4", "8Gi", "10Gi"), "X"),
                2000,
            ),
        ]);
        assert_eq!(s.bound_intents().get("X"), Some(&"n2".to_string()));

        // Exact tie (same creationTimestamp): lexicographically-greater
        // pod name wins, and the result is order-independent.
        let a = with_created(
            with_intent(pod_req("pod-a", Some("n1"), "4", "8Gi", "10Gi"), "X"),
            1000,
        );
        let b = with_created(
            with_intent(pod_req("pod-b", Some("n2"), "4", "8Gi", "10Gi"), "X"),
            1000,
        );
        let fwd = PodSnapshot::derive(&[a.clone(), b.clone()]);
        let rev = PodSnapshot::derive(&[b, a]);
        assert_eq!(fwd.bound_intents().get("X"), rev.bound_intents().get("X"));
        assert_eq!(fwd.bound_intents().get("X"), Some(&"n2".to_string()));
    }
}
