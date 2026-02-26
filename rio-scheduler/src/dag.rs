//! In-memory derivation DAG.
//!
//! The DAG tracks all derivation nodes and their dependency edges across all
//! concurrent builds. Nodes are deduplicated by `drv_hash` (input-addressed:
//! store path; CA: modular derivation hash). Each node tracks which builds
//! are interested in it.

use std::collections::{HashMap, HashSet, VecDeque};

use uuid::Uuid;

use crate::state::{DerivationState, DerivationStatus};

/// Errors from DAG operations.
#[derive(Debug, thiserror::Error)]
pub enum DagError {
    #[error("dependency cycle detected")]
    CycleDetected,
}

/// The global derivation DAG maintained by the actor.
#[derive(Debug)]
pub struct DerivationDag {
    /// All derivation nodes, keyed by drv_hash.
    nodes: HashMap<String, DerivationState>,
    /// Forward edges: parent drv_hash -> set of child drv_hashes.
    /// A "parent" depends on its "children" (children must complete first).
    children: HashMap<String, HashSet<String>>,
    /// Reverse edges: child drv_hash -> set of parent drv_hashes.
    /// Used to find which derivations become ready when a child completes.
    parents: HashMap<String, HashSet<String>>,
    /// Reverse index: drv_path -> drv_hash.
    /// Eliminates O(n) scans in completion handling (gRPC layer receives
    /// drv_path from workers but the DAG is keyed by drv_hash).
    path_to_hash: HashMap<String, String>,
}

impl DerivationDag {
    /// Create an empty DAG.
    pub fn new() -> Self {
        Self {
            nodes: HashMap::new(),
            children: HashMap::new(),
            parents: HashMap::new(),
            path_to_hash: HashMap::new(),
        }
    }

    /// Look up a derivation state by hash.
    pub fn node(&self, drv_hash: &str) -> Option<&DerivationState> {
        self.nodes.get(drv_hash)
    }

    /// Look up a mutable derivation state by hash.
    pub fn node_mut(&mut self, drv_hash: &str) -> Option<&mut DerivationState> {
        self.nodes.get_mut(drv_hash)
    }

    /// Whether a derivation exists in the DAG.
    pub fn contains(&self, drv_hash: &str) -> bool {
        self.nodes.contains_key(drv_hash)
    }

    /// Look up a drv_hash by drv_path (O(1) via reverse index).
    pub fn hash_for_path(&self, drv_path: &str) -> Option<&str> {
        self.path_to_hash.get(drv_path).map(|s| s.as_str())
    }

    /// Iterate all (drv_hash, state) pairs.
    pub fn iter_nodes(&self) -> impl Iterator<Item = (&str, &DerivationState)> {
        self.nodes.iter().map(|(k, v)| (k.as_str(), v))
    }

    /// Iterate all derivation states (without keys).
    pub fn iter_values(&self) -> impl Iterator<Item = &DerivationState> {
        self.nodes.values()
    }

    /// Number of derivation nodes.
    pub fn node_count(&self) -> usize {
        self.nodes.len()
    }

    /// Merge a set of nodes and edges from a new build into the global DAG.
    ///
    /// Returns the list of drv_hashes that were newly inserted (not already present).
    /// Existing nodes get the new `build_id` added to their `interested_builds` set.
    ///
    /// If the merge would create a cycle, returns `Err(DagError::CycleDetected)`
    /// and rolls back all newly-inserted nodes and edges.
    pub fn merge(
        &mut self,
        build_id: Uuid,
        nodes: &[rio_proto::types::DerivationNode],
        edges: &[rio_proto::types::DerivationEdge],
    ) -> Result<Vec<String>, DagError> {
        let mut newly_inserted = Vec::new();
        // Track newly-inserted edges for rollback (pairs of hashes)
        let mut new_edges: Vec<(String, String)> = Vec::new();
        // Track pre-existing nodes that gained interest in this merge, so
        // rollback only removes interest from these (not from nodes where
        // build_id was already present from a prior successful merge).
        let mut interest_added: Vec<String> = Vec::new();

        // Insert or update nodes
        for node in nodes {
            let drv_hash = &node.drv_hash;

            if let Some(existing) = self.nodes.get_mut(drv_hash) {
                // Node already exists: add this build's interest.
                // `insert` returns true iff build_id was not already present.
                if existing.interested_builds.insert(build_id) {
                    interest_added.push(drv_hash.clone());
                }
            } else {
                // New node
                let mut state = DerivationState::from_node(node);
                state.interested_builds.insert(build_id);
                self.path_to_hash
                    .insert(state.drv_path().to_string(), drv_hash.clone());
                self.nodes.insert(drv_hash.clone(), state);
                newly_inserted.push(drv_hash.clone());
            }
        }

        // Insert edges
        for edge in edges {
            // Edges reference drv_path; resolve to drv_hash
            let parent_hash = match self.path_to_hash.get(edge.parent_drv_path.as_str()) {
                Some(h) => h.clone(),
                None => {
                    tracing::warn!(
                        parent_path = %edge.parent_drv_path,
                        "edge references unknown parent drv_path, skipping"
                    );
                    continue;
                }
            };
            let child_hash = match self.path_to_hash.get(edge.child_drv_path.as_str()) {
                Some(h) => h.clone(),
                None => {
                    tracing::warn!(
                        child_path = %edge.child_drv_path,
                        "edge references unknown child drv_path, skipping"
                    );
                    continue;
                }
            };

            let inserted_child = self
                .children
                .entry(parent_hash.clone())
                .or_default()
                .insert(child_hash.clone());
            self.parents
                .entry(child_hash.clone())
                .or_default()
                .insert(parent_hash.clone());

            if inserted_child {
                new_edges.push((parent_hash, child_hash));
            }
        }

        // Cycle check: DFS from each newly-inserted node AND from each parent
        // endpoint of new edges. The latter catches cycles formed by new edges
        // between two pre-existing nodes (no new nodes inserted, so the
        // newly_inserted loop alone would miss them).
        let mut color: HashMap<String, u8> = HashMap::new();
        let mut dfs_starts: Vec<&str> = newly_inserted.iter().map(|s| s.as_str()).collect();
        for (parent, _child) in &new_edges {
            // Only need to DFS from edge endpoints not already covered by
            // newly_inserted (avoids redundant work).
            if !newly_inserted.iter().any(|n| n == parent) {
                dfs_starts.push(parent.as_str());
            }
        }
        for start in dfs_starts {
            if self.has_cycle_from(start, &mut color) {
                // Rollback: remove newly-inserted edges and nodes
                self.rollback_merge(&newly_inserted, &new_edges, &interest_added, build_id);
                return Err(DagError::CycleDetected);
            }
        }

        Ok(newly_inserted)
    }

    /// DFS cycle detection with three-color marking.
    /// color: 0=white (unvisited), 1=gray (in stack), 2=black (done).
    /// A back-edge to a gray node indicates a cycle.
    fn has_cycle_from(&self, start: &str, color: &mut HashMap<String, u8>) -> bool {
        match color.get(start) {
            Some(1) => return true,  // back edge = cycle
            Some(2) => return false, // already fully explored
            _ => {}
        }
        color.insert(start.to_string(), 1);
        if let Some(children) = self.children.get(start) {
            for child in children {
                if self.has_cycle_from(child, color) {
                    return true;
                }
            }
        }
        color.insert(start.to_string(), 2);
        false
    }

    /// Rollback a failed merge: remove newly-inserted nodes and edges, and
    /// remove build interest from pre-existing nodes that gained it during
    /// this merge (but not from nodes where build_id was already present).
    fn rollback_merge(
        &mut self,
        newly_inserted: &[String],
        new_edges: &[(String, String)],
        interest_added: &[String],
        build_id: Uuid,
    ) {
        // Remove newly-inserted edges
        for (parent, child) in new_edges {
            if let Some(children) = self.children.get_mut(parent) {
                children.remove(child);
                if children.is_empty() {
                    self.children.remove(parent);
                }
            }
            if let Some(parents) = self.parents.get_mut(child) {
                parents.remove(parent);
                if parents.is_empty() {
                    self.parents.remove(child);
                }
            }
        }

        // Remove newly-inserted nodes (and their path index entries)
        for hash in newly_inserted {
            if let Some(state) = self.nodes.remove(hash) {
                self.path_to_hash.remove(state.drv_path());
            }
            // Also clean up any edge entries keyed on this hash
            self.children.remove(hash);
            self.parents.remove(hash);
        }

        // Remove build interest only from pre-existing nodes that gained it
        // during THIS merge. Nodes where build_id was already present from a
        // prior successful merge are left untouched.
        for hash in interest_added {
            if let Some(state) = self.nodes.get_mut(hash) {
                state.interested_builds.remove(&build_id);
            }
        }
    }

    /// Check whether all dependencies of a derivation are completed.
    pub fn all_deps_completed(&self, drv_hash: &str) -> bool {
        let children = match self.children.get(drv_hash) {
            Some(c) => c,
            None => return true, // No dependencies
        };

        children.iter().all(|child_hash| {
            self.nodes
                .get(child_hash)
                .is_some_and(|n| n.status() == DerivationStatus::Completed)
        })
    }

    /// Check whether any dependency is in a terminal failure state.
    ///
    /// Used during merge: when a newly inserted node depends on an
    /// already-`Poisoned`/`DependencyFailed` existing node, the new node can
    /// never complete. Without this check it would go to `Queued` and stay
    /// there forever (never Ready since dep != Completed, never cascaded
    /// since cascade only runs on *transition to* Poisoned).
    pub fn any_dep_terminally_failed(&self, drv_hash: &str) -> bool {
        let Some(children) = self.children.get(drv_hash) else {
            return false;
        };
        children.iter().any(|child_hash| {
            self.nodes.get(child_hash).is_some_and(|n| {
                matches!(
                    n.status(),
                    DerivationStatus::Poisoned | DerivationStatus::DependencyFailed
                )
            })
        })
    }

    /// Get all parent drv_hashes that depend on the given child.
    pub fn get_parents(&self, child_hash: &str) -> Vec<String> {
        self.parents
            .get(child_hash)
            .map(|p| p.iter().cloned().collect())
            .unwrap_or_default()
    }

    /// Find all derivations that become ready (all deps completed) after a
    /// given derivation completes. Only returns derivations currently in Queued state.
    pub fn find_newly_ready(&self, completed_hash: &str) -> Vec<String> {
        let mut ready = Vec::new();

        for parent_hash in self.get_parents(completed_hash) {
            if let Some(node) = self.nodes.get(&parent_hash)
                && node.status() == DerivationStatus::Queued
                && self.all_deps_completed(&parent_hash)
            {
                ready.push(parent_hash);
            }
        }

        ready
    }

    /// Get all derivation hashes involved in a build.
    pub fn build_derivations(&self, build_id: Uuid) -> Vec<String> {
        self.nodes
            .iter()
            .filter(|(_, state)| state.interested_builds.contains(&build_id))
            .map(|(hash, _)| hash.clone())
            .collect()
    }

    /// Remove a build's interest from all its derivations.
    /// Returns derivation hashes that are now orphaned (no builds interested).
    pub fn remove_build_interest(&mut self, build_id: Uuid) -> Vec<String> {
        let mut orphaned = Vec::new();

        for (hash, state) in &mut self.nodes {
            state.interested_builds.remove(&build_id);
            if state.interested_builds.is_empty() && !state.status().is_terminal() {
                orphaned.push(hash.clone());
            }
        }

        orphaned
    }

    /// Remove a build's interest from all its derivations, and reap (delete)
    /// any nodes that are now orphaned (no builds interested) AND in a terminal
    /// state. Returns the number of nodes reaped.
    ///
    /// This prevents unbounded DAG growth for long-running schedulers.
    /// Non-terminal orphaned nodes are preserved (they may be mid-build for
    /// a different code path, though this shouldn't happen in practice).
    pub fn remove_build_interest_and_reap(&mut self, build_id: Uuid) -> usize {
        let mut to_reap = Vec::new();

        for (hash, state) in &mut self.nodes {
            state.interested_builds.remove(&build_id);
            if state.interested_builds.is_empty() && state.status().is_terminal() {
                to_reap.push(hash.clone());
            }
        }

        let reaped = to_reap.len();
        for hash in to_reap {
            if let Some(state) = self.nodes.remove(&hash) {
                self.path_to_hash.remove(state.drv_path());
            }
            self.children.remove(&hash);
            self.parents.remove(&hash);
            // Also scrub this hash from other nodes' edge sets.
            for children in self.children.values_mut() {
                children.remove(&hash);
            }
            for parents in self.parents.values_mut() {
                parents.remove(&hash);
            }
        }

        reaped
    }

    /// Determine initial states for newly merged derivations.
    ///
    /// Derivations with no incomplete dependencies go to Queued, then
    /// immediately to Ready. Others stay in Created until they become Queued
    /// (when their dependencies complete).
    ///
    /// Returns lists of (drv_hash, new_status) transitions.
    pub fn compute_initial_states(
        &self,
        newly_inserted: &[String],
    ) -> Vec<(String, DerivationStatus)> {
        let mut transitions = Vec::new();

        for drv_hash in newly_inserted {
            if self.all_deps_completed(drv_hash) {
                // No deps or all deps already completed -> directly to ready
                // We go created -> queued -> ready
                transitions.push((drv_hash.clone(), DerivationStatus::Ready));
            } else if self.any_dep_terminally_failed(drv_hash) {
                // A dep is already poisoned/failed. This node cannot complete.
                // Mark DependencyFailed so the build terminates instead of
                // hanging forever with this node stuck in Queued.
                transitions.push((drv_hash.clone(), DerivationStatus::DependencyFailed));
            } else {
                // Has incomplete deps -> queued (waiting for deps)
                transitions.push((drv_hash.clone(), DerivationStatus::Queued));
            }
        }

        transitions
    }

    /// Find all leaf derivations (no dependencies) for a build.
    pub fn find_leaves(&self, build_id: Uuid) -> Vec<String> {
        self.nodes
            .iter()
            .filter(|(_, state)| state.interested_builds.contains(&build_id))
            .filter(|(hash, _)| {
                self.children
                    .get(hash.as_str())
                    .is_none_or(|c| c.is_empty())
            })
            .map(|(hash, _)| hash.clone())
            .collect()
    }

    /// Find all root derivations (no parents) for a build.
    /// These are the top-level derivations the client actually wants built.
    pub fn find_roots(&self, build_id: Uuid) -> Vec<String> {
        self.nodes
            .iter()
            .filter(|(_, state)| state.interested_builds.contains(&build_id))
            .filter(|(hash, _)| self.parents.get(hash.as_str()).is_none_or(|p| p.is_empty()))
            .map(|(hash, _)| hash.clone())
            .collect()
    }

    /// Compute summary counts for a build.
    pub fn build_summary(&self, build_id: Uuid) -> BuildSummary {
        let mut summary = BuildSummary::default();

        for state in self.nodes.values() {
            if !state.interested_builds.contains(&build_id) {
                continue;
            }
            summary.total += 1;
            match state.status() {
                DerivationStatus::Completed => summary.completed += 1,
                DerivationStatus::Running => summary.running += 1,
                DerivationStatus::Assigned => summary.running += 1,
                DerivationStatus::Failed
                | DerivationStatus::Poisoned
                | DerivationStatus::DependencyFailed => summary.failed += 1,
                DerivationStatus::Ready | DerivationStatus::Queued | DerivationStatus::Created => {
                    summary.queued += 1;
                }
            }
        }

        summary
    }

    /// Perform a topological walk from roots, yielding nodes in dependency order.
    /// This is BFS from leaves toward roots (Kahn's algorithm).
    pub fn topological_order(&self) -> Vec<String> {
        // Compute dep counts: number of children (deps) for each node
        let mut dep_count: HashMap<&str, usize> = HashMap::new();
        for hash in self.nodes.keys() {
            let count = self.children.get(hash).map(|c| c.len()).unwrap_or(0);
            dep_count.insert(hash.as_str(), count);
        }

        let mut queue: VecDeque<&str> = dep_count
            .iter()
            .filter(|(_, count)| **count == 0)
            .map(|(&hash, _)| hash)
            .collect();

        let mut order = Vec::new();
        while let Some(hash) = queue.pop_front() {
            order.push(hash.to_string());
            // For each parent that depends on this hash
            if let Some(parent_hashes) = self.parents.get(hash) {
                for parent in parent_hashes {
                    if let Some(count) = dep_count.get_mut(parent.as_str()) {
                        *count = count.saturating_sub(1);
                        if *count == 0 {
                            queue.push_back(parent.as_str());
                        }
                    }
                }
            }
        }

        order
    }
}

impl Default for DerivationDag {
    fn default() -> Self {
        Self::new()
    }
}

/// Summary counts for a build's derivations.
#[derive(Debug, Default, Clone)]
pub struct BuildSummary {
    pub total: u32,
    pub completed: u32,
    pub running: u32,
    pub failed: u32,
    pub queued: u32,
}

#[cfg(test)]
mod tests {
    use super::*;
    use rio_proto::types::{DerivationEdge, DerivationNode};

    fn make_node(drv_hash: &str, drv_path: &str, system: &str) -> DerivationNode {
        DerivationNode {
            drv_path: drv_path.to_string(),
            drv_hash: drv_hash.to_string(),
            pname: String::new(),
            system: system.to_string(),
            required_features: vec![],
            output_names: vec!["out".to_string()],
            is_fixed_output: false,
            expected_output_paths: vec![],
        }
    }

    fn make_edge(parent: &str, child: &str) -> DerivationEdge {
        DerivationEdge {
            parent_drv_path: parent.to_string(),
            child_drv_path: child.to_string(),
        }
    }

    #[test]
    fn test_merge_empty_dag() {
        let mut dag = DerivationDag::new();
        let build_id = Uuid::new_v4();
        let nodes = vec![make_node("hash1", "/nix/store/hash1.drv", "x86_64-linux")];
        let edges = vec![];

        let newly = dag.merge(build_id, &nodes, &edges).unwrap();
        assert_eq!(newly.len(), 1);
        assert!(dag.nodes.contains_key("hash1"));
        assert!(dag.nodes["hash1"].interested_builds.contains(&build_id));
    }

    #[test]
    fn test_merge_dedup() {
        let mut dag = DerivationDag::new();
        let build1 = Uuid::new_v4();
        let build2 = Uuid::new_v4();
        let nodes = vec![make_node("hash1", "/nix/store/hash1.drv", "x86_64-linux")];

        let newly1 = dag.merge(build1, &nodes, &[]).unwrap();
        assert_eq!(newly1.len(), 1);

        let newly2 = dag.merge(build2, &nodes, &[]).unwrap();
        assert_eq!(newly2.len(), 0); // Already exists

        let node = &dag.nodes["hash1"];
        assert!(node.interested_builds.contains(&build1));
        assert!(node.interested_builds.contains(&build2));
    }

    #[test]
    fn test_edges_and_deps() {
        let mut dag = DerivationDag::new();
        let build_id = Uuid::new_v4();
        let nodes = vec![
            make_node("hashA", "/nix/store/a.drv", "x86_64-linux"),
            make_node("hashB", "/nix/store/b.drv", "x86_64-linux"),
            make_node("hashC", "/nix/store/c.drv", "x86_64-linux"),
        ];
        // A depends on B and C
        let edges = vec![
            make_edge("/nix/store/a.drv", "/nix/store/b.drv"),
            make_edge("/nix/store/a.drv", "/nix/store/c.drv"),
        ];

        dag.merge(build_id, &nodes, &edges).unwrap();

        // A has deps, B and C don't
        assert!(!dag.all_deps_completed("hashA"));
        assert!(dag.all_deps_completed("hashB"));
        assert!(dag.all_deps_completed("hashC"));

        // Check parent/child relationships
        assert_eq!(dag.children["hashA"].len(), 2);
        assert!(dag.get_parents("hashB").contains(&"hashA".to_string()));
        assert!(dag.get_parents("hashC").contains(&"hashA".to_string()));
    }

    #[test]
    fn test_initial_states() {
        let mut dag = DerivationDag::new();
        let build_id = Uuid::new_v4();
        let nodes = vec![
            make_node("hashA", "/nix/store/a.drv", "x86_64-linux"),
            make_node("hashB", "/nix/store/b.drv", "x86_64-linux"),
        ];
        let edges = vec![make_edge("/nix/store/a.drv", "/nix/store/b.drv")];

        let newly = dag.merge(build_id, &nodes, &edges).unwrap();
        let states = dag.compute_initial_states(&newly);

        // B has no deps -> Ready; A has dep on B -> Queued
        for (hash, status) in &states {
            if hash == "hashB" {
                assert_eq!(*status, DerivationStatus::Ready);
            } else if hash == "hashA" {
                assert_eq!(*status, DerivationStatus::Queued);
            }
        }
    }

    #[test]
    fn test_initial_states_with_prepoisoned_dep() {
        let mut dag = DerivationDag::new();
        let build1 = Uuid::new_v4();

        // Build 1: just the leaf.
        let leaf_nodes = vec![make_node("leafP", "/nix/store/leafP.drv", "x86_64-linux")];
        dag.merge(build1, &leaf_nodes, &[]).unwrap();

        // Poison it.
        dag.nodes
            .get_mut("leafP")
            .unwrap()
            .set_status_for_test(DerivationStatus::Poisoned);

        assert!(!dag.any_dep_terminally_failed("leafP")); // no deps

        // Build 2: parent depending on the poisoned leaf.
        let build2 = Uuid::new_v4();
        let parent_nodes = vec![
            make_node("parentP", "/nix/store/parentP.drv", "x86_64-linux"),
            make_node("leafP", "/nix/store/leafP.drv", "x86_64-linux"),
        ];
        let edges = vec![make_edge("/nix/store/parentP.drv", "/nix/store/leafP.drv")];
        let newly = dag.merge(build2, &parent_nodes, &edges).unwrap();

        // Only parentP is newly inserted (leafP already existed).
        assert_eq!(newly, vec!["parentP"]);
        assert!(dag.any_dep_terminally_failed("parentP"));

        // compute_initial_states should return DependencyFailed for parentP.
        let states = dag.compute_initial_states(&newly);
        assert_eq!(states.len(), 1);
        assert_eq!(states[0].0, "parentP");
        assert_eq!(
            states[0].1,
            DerivationStatus::DependencyFailed,
            "node with pre-poisoned dep should be DependencyFailed, not Queued"
        );
    }

    #[test]
    fn test_find_newly_ready() {
        let mut dag = DerivationDag::new();
        let build_id = Uuid::new_v4();
        let nodes = vec![
            make_node("hashA", "/nix/store/a.drv", "x86_64-linux"),
            make_node("hashB", "/nix/store/b.drv", "x86_64-linux"),
        ];
        let edges = vec![make_edge("/nix/store/a.drv", "/nix/store/b.drv")];

        dag.merge(build_id, &nodes, &edges).unwrap();

        // Set B to completed, A to queued
        dag.nodes
            .get_mut("hashB")
            .unwrap()
            .set_status_for_test(DerivationStatus::Completed);
        dag.nodes
            .get_mut("hashA")
            .unwrap()
            .set_status_for_test(DerivationStatus::Queued);

        let ready = dag.find_newly_ready("hashB");
        assert_eq!(ready, vec!["hashA".to_string()]);
    }

    #[test]
    fn test_topological_order() {
        let mut dag = DerivationDag::new();
        let build_id = Uuid::new_v4();
        // C depends on B, B depends on A
        let nodes = vec![
            make_node("hashA", "/nix/store/a.drv", "x86_64-linux"),
            make_node("hashB", "/nix/store/b.drv", "x86_64-linux"),
            make_node("hashC", "/nix/store/c.drv", "x86_64-linux"),
        ];
        let edges = vec![
            make_edge("/nix/store/c.drv", "/nix/store/b.drv"),
            make_edge("/nix/store/b.drv", "/nix/store/a.drv"),
        ];

        dag.merge(build_id, &nodes, &edges).unwrap();
        let order = dag.topological_order();

        // A must come before B, B before C
        let pos_a = order.iter().position(|h| h == "hashA").unwrap();
        let pos_b = order.iter().position(|h| h == "hashB").unwrap();
        let pos_c = order.iter().position(|h| h == "hashC").unwrap();
        assert!(pos_a < pos_b);
        assert!(pos_b < pos_c);
    }

    // -----------------------------------------------------------------------
    // Group 2: Cycle detection
    // -----------------------------------------------------------------------

    /// A cyclic DAG should be rejected, with all newly-inserted nodes rolled back.
    #[test]
    fn test_merge_rejects_cycle() {
        let mut dag = DerivationDag::new();
        let build_id = Uuid::new_v4();

        // A depends on B, B depends on A — cycle
        let nodes = vec![
            make_node("hashA", "/nix/store/a.drv", "x86_64-linux"),
            make_node("hashB", "/nix/store/b.drv", "x86_64-linux"),
        ];
        let edges = vec![
            make_edge("/nix/store/a.drv", "/nix/store/b.drv"),
            make_edge("/nix/store/b.drv", "/nix/store/a.drv"), // cycle!
        ];

        let result = dag.merge(build_id, &nodes, &edges);
        assert!(result.is_err(), "cyclic DAG should be rejected");
        assert_eq!(
            dag.nodes.len(),
            0,
            "no nodes should remain after cycle rollback"
        );
        assert_eq!(dag.children.len(), 0, "edges should be rolled back");
        assert_eq!(dag.parents.len(), 0, "edges should be rolled back");
    }

    /// An indirect cycle (A -> B -> C -> A) should also be detected.
    #[test]
    fn test_merge_rejects_indirect_cycle() {
        let mut dag = DerivationDag::new();
        let build_id = Uuid::new_v4();

        let nodes = vec![
            make_node("hashA", "/nix/store/a.drv", "x86_64-linux"),
            make_node("hashB", "/nix/store/b.drv", "x86_64-linux"),
            make_node("hashC", "/nix/store/c.drv", "x86_64-linux"),
        ];
        // A depends on B, B depends on C, C depends on A — indirect cycle
        let edges = vec![
            make_edge("/nix/store/a.drv", "/nix/store/b.drv"),
            make_edge("/nix/store/b.drv", "/nix/store/c.drv"),
            make_edge("/nix/store/c.drv", "/nix/store/a.drv"),
        ];

        let result = dag.merge(build_id, &nodes, &edges);
        assert!(result.is_err(), "indirect cycle should be rejected");
        assert_eq!(dag.nodes.len(), 0);
    }

    /// A valid DAG merged after a cycle-rejected attempt should succeed.
    #[test]
    fn test_merge_after_cycle_rollback() {
        let mut dag = DerivationDag::new();
        let build_id = Uuid::new_v4();

        // First: try to insert a cycle (should fail and rollback)
        let cyclic_nodes = vec![
            make_node("hashA", "/nix/store/a.drv", "x86_64-linux"),
            make_node("hashB", "/nix/store/b.drv", "x86_64-linux"),
        ];
        let cyclic_edges = vec![
            make_edge("/nix/store/a.drv", "/nix/store/b.drv"),
            make_edge("/nix/store/b.drv", "/nix/store/a.drv"),
        ];
        assert!(dag.merge(build_id, &cyclic_nodes, &cyclic_edges).is_err());

        // Second: insert a valid DAG with the same nodes (should succeed)
        let valid_edges = vec![make_edge("/nix/store/a.drv", "/nix/store/b.drv")];
        let result = dag.merge(build_id, &cyclic_nodes, &valid_edges);
        assert!(
            result.is_ok(),
            "valid merge after rollback should succeed: {result:?}"
        );
        assert_eq!(dag.nodes.len(), 2);
    }

    /// A new edge between two PRE-EXISTING nodes (no new nodes inserted)
    /// can create a cycle. The DFS must start from edge endpoints, not just
    /// newly-inserted nodes.
    #[test]
    fn test_cycle_via_new_edge_between_existing_nodes() {
        let mut dag = DerivationDag::new();
        let build1 = Uuid::new_v4();

        // Insert A and B separately with A->B edge.
        let nodes = vec![
            make_node("hashA", "/nix/store/a.drv", "x86_64-linux"),
            make_node("hashB", "/nix/store/b.drv", "x86_64-linux"),
        ];
        let initial_edges = vec![make_edge("/nix/store/a.drv", "/nix/store/b.drv")];
        dag.merge(build1, &nodes, &initial_edges).unwrap();
        assert_eq!(dag.nodes.len(), 2);

        // Now merge the SAME nodes (no new inserts) with a B->A edge.
        // This creates a cycle via a new edge between two existing nodes.
        let build2 = Uuid::new_v4();
        let cycle_edge = vec![make_edge("/nix/store/b.drv", "/nix/store/a.drv")];
        let result = dag.merge(build2, &nodes, &cycle_edge);

        assert!(
            result.is_err(),
            "cycle via new edge between existing nodes should be detected"
        );
        // Rollback: the new edge should be gone, but the original A->B stays.
        assert!(
            dag.children
                .get("hashA")
                .is_some_and(|c| c.contains("hashB")),
            "original A->B edge should survive rollback"
        );
        assert!(
            !dag.children
                .get("hashB")
                .is_some_and(|c| c.contains("hashA")),
            "cycle-creating B->A edge should be rolled back"
        );
    }

    /// When a build merges successfully, then later merges again with a cycle,
    /// rollback must NOT clear the build's interest from nodes that already
    /// had it from the prior successful merge.
    #[test]
    fn test_cycle_rollback_preserves_prior_interest() {
        let mut dag = DerivationDag::new();
        let b1 = Uuid::new_v4();

        // Step 1: merge B1 with node A only — succeeds. A.interested = {B1}.
        let nodes_a = vec![make_node("hashA", "/nix/store/a.drv", "x86_64-linux")];
        dag.merge(b1, &nodes_a, &[]).unwrap();
        assert!(
            dag.nodes
                .get("hashA")
                .unwrap()
                .interested_builds
                .contains(&b1),
            "B1 interest in A should be set after successful merge"
        );

        // Step 2: merge B1 again with nodes {A, C} and cycle A->C->A — fails.
        // Pre-fix: rollback would clear B1 from A even though B1 was already
        // interested in A from step 1.
        let nodes_ac = vec![
            make_node("hashA", "/nix/store/a.drv", "x86_64-linux"),
            make_node("hashC", "/nix/store/c.drv", "x86_64-linux"),
        ];
        let cycle_edges = vec![
            make_edge("/nix/store/a.drv", "/nix/store/c.drv"),
            make_edge("/nix/store/c.drv", "/nix/store/a.drv"),
        ];
        let result = dag.merge(b1, &nodes_ac, &cycle_edges);
        assert!(result.is_err(), "cycle should be rejected");

        // Step 3: A should STILL have B1 interest (was present before the
        // failed merge). C should be gone entirely (was newly inserted).
        assert!(
            dag.nodes
                .get("hashA")
                .unwrap()
                .interested_builds
                .contains(&b1),
            "B1 interest in A from prior successful merge must survive rollback"
        );
        assert!(
            !dag.nodes.contains_key("hashC"),
            "newly-inserted C should be rolled back"
        );
    }

    /// The path_to_hash reverse index must stay in sync with nodes across
    /// merge, rollback, and reap operations.
    #[test]
    fn test_path_to_hash_consistency() {
        let mut dag = DerivationDag::new();
        let b1 = Uuid::new_v4();

        // Merge: index should be populated.
        let nodes = vec![
            make_node("hashA", "/nix/store/a.drv", "x86_64-linux"),
            make_node("hashB", "/nix/store/b.drv", "x86_64-linux"),
        ];
        dag.merge(b1, &nodes, &[]).unwrap();
        assert_eq!(dag.hash_for_path("/nix/store/a.drv"), Some("hashA"));
        assert_eq!(dag.hash_for_path("/nix/store/b.drv"), Some("hashB"));
        assert_eq!(dag.hash_for_path("/nix/store/nonexistent.drv"), None);

        // Cycle rollback: newly-inserted node's path entry must be removed.
        let cycle_nodes = vec![
            make_node("hashA", "/nix/store/a.drv", "x86_64-linux"),
            make_node("hashC", "/nix/store/c.drv", "x86_64-linux"),
        ];
        let cycle_edges = vec![
            make_edge("/nix/store/a.drv", "/nix/store/c.drv"),
            make_edge("/nix/store/c.drv", "/nix/store/a.drv"),
        ];
        dag.merge(b1, &cycle_nodes, &cycle_edges).unwrap_err();
        assert_eq!(
            dag.hash_for_path("/nix/store/c.drv"),
            None,
            "rollback must remove path index for newly-inserted node"
        );
        assert_eq!(
            dag.hash_for_path("/nix/store/a.drv"),
            Some("hashA"),
            "rollback must preserve path index for pre-existing node"
        );

        // Reap: terminal orphaned node's path entry must be removed.
        // First, mark A as terminal so it's eligible for reaping.
        dag.node_mut("hashA")
            .unwrap()
            .transition(DerivationStatus::Queued)
            .unwrap();
        dag.node_mut("hashA")
            .unwrap()
            .transition(DerivationStatus::Ready)
            .unwrap();
        dag.node_mut("hashA")
            .unwrap()
            .transition(DerivationStatus::Assigned)
            .unwrap();
        dag.node_mut("hashA")
            .unwrap()
            .transition(DerivationStatus::Running)
            .unwrap();
        dag.node_mut("hashA")
            .unwrap()
            .transition(DerivationStatus::Completed)
            .unwrap();
        let reaped = dag.remove_build_interest_and_reap(b1);
        assert_eq!(reaped, 1, "hashA should be reaped (terminal, no interest)");
        assert_eq!(
            dag.hash_for_path("/nix/store/a.drv"),
            None,
            "reap must remove path index for reaped node"
        );
        // B is not terminal, so it survives reaping.
        assert_eq!(dag.hash_for_path("/nix/store/b.drv"), Some("hashB"));
    }
}
