//! In-memory derivation DAG.
//!
//! The DAG tracks all derivation nodes and their dependency edges across all
//! concurrent builds. Nodes are deduplicated by `drv_hash` (input-addressed:
//! store path; CA: modular derivation hash). Each node tracks which builds
//! are interested in it.

use std::collections::{BTreeSet, HashMap, HashSet};

use uuid::Uuid;

use crate::state::{DerivationState, DerivationStatus, DrvHash};

/// CA-cutoff cascade node-count cap. Bounds work on pathological DAGs
/// (e.g., a linear chain of 100k Queued nodes would otherwise walk
/// the whole thing synchronously inside a single completion handler).
/// Each iteration is one `find_cutoff_eligible_speculative` call
/// (O(fanout × children)), so 1000 iterations × typical fanout ≈
/// low-thousands of nodes skipped per cascade — ample for real-world
/// DAGs, bounded for adversarial ones. This is a NODE-COUNT cap (pops
/// from the frontier stack), not a BFS tree-depth cap.
pub const MAX_CASCADE_NODES: usize = 1000;

/// Errors from DAG operations.
#[derive(Debug, thiserror::Error)]
pub enum DagError {
    #[error("dependency cycle detected")]
    CycleDetected,
    #[error("invalid drv_path {path:?}: {source}")]
    InvalidDrvPath {
        path: String,
        source: rio_nix::store_path::StorePathError,
    },
}

/// Result of a successful `merge()` operation. Surfaces all the rollback
/// state that `merge()` already tracks internally, so callers can invoke
/// `rollback_merge()` if their own post-merge persistence fails.
#[derive(Debug)]
pub struct MergeResult {
    /// Hashes of nodes newly inserted by this merge (not pre-existing).
    pub newly_inserted: HashSet<DrvHash>,
    /// Edges newly added by this merge as (parent_hash, child_hash) pairs.
    pub new_edges: Vec<(DrvHash, DrvHash)>,
    /// Hashes of pre-existing nodes that gained build_id interest.
    /// Rollback removes interest only from these (not from nodes where
    /// build_id was already present from a prior merge).
    pub interest_added: Vec<DrvHash>,
}

/// The global derivation DAG maintained by the actor.
#[derive(Debug, Default)]
pub struct DerivationDag {
    /// All derivation nodes, keyed by drv_hash.
    nodes: HashMap<DrvHash, DerivationState>,
    /// Forward edges: parent drv_hash -> set of child drv_hashes.
    /// A "parent" depends on its "children" (children must complete first).
    children: HashMap<DrvHash, HashSet<DrvHash>>,
    /// Reverse edges: child drv_hash -> set of parent drv_hashes.
    /// Used to find which derivations become ready when a child completes.
    parents: HashMap<DrvHash, HashSet<DrvHash>>,
    /// Reverse index: drv_path -> drv_hash.
    /// Eliminates O(n) scans in completion handling (gRPC layer receives
    /// drv_path from workers but the DAG is keyed by drv_hash).
    path_to_hash: HashMap<String, DrvHash>,
}

impl DerivationDag {
    /// Create an empty DAG.
    pub fn new() -> Self {
        Self::default()
    }

    /// Insert a pre-built node (Phase 3b state recovery). No cycle
    /// check — recovered edges come from PG which was validated at
    /// merge time. No interested_builds population — that's done
    /// separately from the build_derivations join.
    ///
    /// If the node already exists (shouldn't — recover_from_pg
    /// clears the DAG first), the existing one is kept (first-wins,
    /// no overwrite). warn! since it indicates a double-insert bug.
    pub fn insert_recovered_node(&mut self, state: DerivationState) {
        let hash = state.drv_hash.clone();
        if self.nodes.contains_key(&hash) {
            tracing::warn!(drv_hash = %hash, "duplicate recovered node (skipping)");
            return;
        }
        self.path_to_hash
            .insert(state.drv_path().to_string(), hash.clone());
        self.nodes.insert(hash, state);
    }

    /// Insert a recovered edge. No cycle check (PG was validated
    /// at merge time). Idempotent — HashSet::insert is a no-op for
    /// existing entries.
    pub fn insert_recovered_edge(&mut self, parent: DrvHash, child: DrvHash) {
        self.children
            .entry(parent.clone())
            .or_default()
            .insert(child.clone());
        self.parents.entry(child).or_default().insert(parent);
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
    pub fn hash_for_path(&self, drv_path: &str) -> Option<&DrvHash> {
        self.path_to_hash.get(drv_path)
    }

    /// Returns a clone of the canonical `DrvHash` stored as a key in `nodes`.
    ///
    /// "Canonical" means: the Arc allocated when the node was FIRST inserted
    /// by `merge()`. All subsequent clones via `path_to_hash`, `children`,
    /// `parents`, `get_parents`/`get_children`, `compute_initial_states`, etc.
    /// share this same Arc (ptr-equal). Cloning the canonical DrvHash is an
    /// atomic refcount bump — no allocation.
    ///
    /// Returns `None` if the hash isn't in the DAG. The `&str` signature
    /// accepts both raw strings and `&DrvHash` (via deref coercion).
    ///
    /// Use this when you have a FRESH `DrvHash` (e.g., constructed from a
    /// proto string) and want to exchange it for the existing canonical one,
    /// so downstream clones don't proliferate distinct Arc allocations.
    pub fn canonical(&self, hash: &str) -> Option<DrvHash> {
        self.nodes.get_key_value(hash).map(|(k, _)| k.clone())
    }

    /// Iterate all (drv_hash, state) pairs.
    pub fn iter_nodes(&self) -> impl Iterator<Item = (&str, &DerivationState)> {
        self.nodes.iter().map(|(k, v)| (k.as_str(), v))
    }

    /// Iterate all derivation states (without keys).
    pub fn iter_values(&self) -> impl Iterator<Item = &DerivationState> {
        self.nodes.values()
    }

    /// Merge a set of nodes and edges from a new build into the global DAG.
    ///
    /// Returns a `MergeResult` with the hashes of newly-inserted nodes,
    /// newly-added edges, and pre-existing nodes that gained build interest.
    /// Existing nodes get the new `build_id` added to `interested_builds`.
    ///
    /// If the merge would create a cycle, returns `Err(DagError::CycleDetected)`
    /// and rolls back all newly-inserted nodes/edges/interest.
    ///
    /// Callers that perform additional persistence (e.g., DB writes) after a
    /// successful merge should call `rollback_merge()` with the returned
    /// `MergeResult` fields if their persistence fails, to avoid in-memory
    /// DAG state drifting from the DB.
    pub fn merge(
        &mut self,
        build_id: Uuid,
        nodes: &[rio_proto::dag::DerivationNode],
        edges: &[rio_proto::dag::DerivationEdge],
        submitter_traceparent: &str,
    ) -> Result<MergeResult, DagError> {
        let mut newly_inserted = HashSet::new();
        // Track newly-inserted edges for rollback (pairs of hashes)
        let mut new_edges: Vec<(DrvHash, DrvHash)> = Vec::new();
        // Track pre-existing nodes that gained interest in this merge, so
        // rollback only removes interest from these (not from nodes where
        // build_id was already present from a prior successful merge).
        let mut interest_added: Vec<DrvHash> = Vec::new();

        // Insert or update nodes
        for node in nodes {
            // Exchange the fresh proto-derived hash for the canonical one
            // from a prior merge if this node already exists. canonical()
            // returns an Arc-clone of the existing map key, so downstream
            // pushes into interest_added don't proliferate distinct allocs.
            // If the node is new, our fresh Arc BECOMES the canonical one
            // when inserted below.
            let drv_hash = self
                .canonical(node.drv_hash.as_str())
                .unwrap_or_else(|| node.drv_hash.as_str().into());

            // Resubmit-retry: if the existing node is Cancelled or Failed,
            // remove it so the else-branch below re-inserts fresh state and
            // it flows through `compute_initial_states` → `newly_inserted`.
            // Without this, a Cancelled node left by a timed-out build
            // (reap misses it — `cancel_build_derivations` removes interest
            // BEFORE `remove_build_interest_and_reap`'s `was_interested`
            // check) makes the resubmitted build hang: merge adds interest
            // but `compute_initial_states` only iterates newly-inserted, and
            // `handle_merge_dag`'s pre-existing-node match ignores Cancelled.
            //
            // Edges are NOT scrubbed: `children`/`parents` are keyed by hash
            // string, so they stay valid across the remove+reinsert. The merge's
            // own edge loop re-adds this submission's edges idempotently.
            //
            // Prior interested_builds are carried over so any OTHER build
            // that was stuck on this Cancelled node also benefits from the
            // reset.
            let prior_interest = if self
                .nodes
                .get(&drv_hash)
                .is_some_and(|n| n.status().is_retriable_on_resubmit())
            {
                self.nodes
                    .remove(&drv_hash)
                    .map(|old| old.interested_builds)
            } else {
                None
            };

            if let Some(existing) = self.nodes.get_mut(&drv_hash) {
                // Node already exists: add this build's interest.
                // `insert` returns true iff build_id was not already present.
                if existing.interested_builds.insert(build_id) {
                    interest_added.push(drv_hash);
                }
                // First submitter's traceparent wins — but recovery/
                // poison-reset set "", which isn't a submitter. Upgrade
                // an empty traceparent so a user submitting after
                // failover gets their trace linked to the worker span.
                if existing.traceparent.is_empty() && !submitter_traceparent.is_empty() {
                    existing.traceparent = submitter_traceparent.to_string();
                }
            } else {
                // New node. try_from_node validates drv_path: StorePath::parse.
                // Invalid paths fail the whole merge (the actor rolls back
                // as it does for CycleDetected). The gRPC layer also validates
                // upfront and returns INVALID_ARGUMENT, so this is belt-and-
                // suspenders — but it's the only thing protecting us if the
                // actor is ever driven by something other than the gRPC layer.
                let mut state =
                    DerivationState::try_from_node(node).map_err(|e| DagError::InvalidDrvPath {
                        path: node.drv_path.clone(),
                        source: e,
                    })?;
                state.interested_builds.insert(build_id);
                // Carry over interested_builds from the removed retriable
                // node (if any) — other stuck builds get the reset too.
                if let Some(prior) = prior_interest {
                    state.interested_builds.extend(prior);
                }
                state.traceparent = submitter_traceparent.to_string();
                // All three inserts clone the SAME Arc — they're mutually
                // ptr-equal. This is what makes path_to_hash.get().cloned()
                // canonical, which makes the edge-insert loop canonical, etc.
                self.path_to_hash
                    .insert(state.drv_path().to_string(), drv_hash.clone());
                self.nodes.insert(drv_hash.clone(), state);
                newly_inserted.insert(drv_hash);
            }
        }

        // Insert edges
        for edge in edges {
            // Edges reference drv_path; resolve to drv_hash
            let Some(parent_hash) = self
                .path_to_hash
                .get(edge.parent_drv_path.as_str())
                .cloned()
            else {
                tracing::warn!(
                    parent_path = %edge.parent_drv_path,
                    "edge references unknown parent drv_path, skipping"
                );
                continue;
            };
            let Some(child_hash) = self.path_to_hash.get(edge.child_drv_path.as_str()).cloned()
            else {
                tracing::warn!(
                    child_path = %edge.child_drv_path,
                    "edge references unknown child drv_path, skipping"
                );
                continue;
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
            if !newly_inserted.contains(parent) {
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

        Ok(MergeResult {
            newly_inserted,
            new_edges,
            interest_added,
        })
    }

    /// Iterative DFS cycle detection with three-color marking.
    /// color: 0=white (unvisited), 1=gray (in stack), 2=black (done).
    /// A back-edge to a gray node indicates a cycle.
    ///
    /// Iterative (not recursive) to avoid stack overflow on deep chains:
    /// MAX_DAG_NODES=1M × ~150B/frame ≈ 150MB >> 2MB default tokio stack.
    fn has_cycle_from(&self, start: &str, color: &mut HashMap<String, u8>) -> bool {
        // Short-circuit if start already visited (color map persists across
        // multiple has_cycle_from calls in merge()).
        match color.get(start) {
            Some(1) => return true,
            Some(2) => return false,
            _ => {}
        }

        // Explicit stack: each frame is (node, children_iterator_state).
        // We store the child vec snapshot + index to resume iteration after
        // returning from a child's subtree.
        // Using Vec<String> snapshots avoids borrow-checker conflicts with
        // `color.insert(node.to_string(), ...)` inside the loop.
        struct Frame {
            node: String,
            children: Vec<DrvHash>,
            next_child: usize,
        }

        let children_of = |n: &str| -> Vec<DrvHash> {
            self.children
                .get(n)
                .map(|set| set.iter().cloned().collect())
                .unwrap_or_default()
        };

        let mut stack: Vec<Frame> = vec![Frame {
            node: start.to_string(),
            children: children_of(start),
            next_child: 0,
        }];
        color.insert(start.to_string(), 1);

        while let Some(frame) = stack.last_mut() {
            if frame.next_child < frame.children.len() {
                let child = frame.children[frame.next_child].clone();
                frame.next_child += 1;

                match color.get(child.as_str()) {
                    Some(1) => return true, // back edge → cycle
                    Some(2) => continue,    // already fully explored
                    _ => {
                        // Unvisited: push a new frame (descend).
                        let grandchildren = children_of(&child);
                        let node = child.to_string();
                        color.insert(node.clone(), 1);
                        stack.push(Frame {
                            node,
                            children: grandchildren,
                            next_child: 0,
                        });
                    }
                }
            } else {
                // All children visited: mark black and pop.
                let done = stack.pop().expect("stack nonempty in else branch");
                color.insert(done.node, 2);
            }
        }

        false
    }

    /// Rollback a failed merge: remove newly-inserted nodes and edges, and
    /// remove build interest from pre-existing nodes that gained it during
    /// this merge (but not from nodes where build_id was already present).
    pub(crate) fn rollback_merge(
        &mut self,
        newly_inserted: &HashSet<DrvHash>,
        new_edges: &[(DrvHash, DrvHash)],
        interest_added: &[DrvHash],
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
                self.path_to_hash.remove(state.drv_path().as_str());
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

    // r[impl sched.state.transitions]
    /// Check whether all dependencies of a derivation are satisfied
    /// (output available). `Completed` and `Skipped` are both
    /// acceptable: `Skipped` means CA cutoff verified the output
    /// already exists in the store (see [`Self::cascade_cutoff`]'s
    /// verify closure). A dep in any other state — including other
    /// terminal states like `Poisoned`/`DependencyFailed`/`Cancelled`
    /// — means the output is NOT available; the caller should route
    /// through [`Self::any_dep_terminally_failed`] instead (cascades
    /// `DependencyFailed` rather than promoting to `Ready`).
    pub fn all_deps_completed(&self, drv_hash: &str) -> bool {
        let Some(children) = self.children.get(drv_hash) else {
            return true; // No dependencies
        };

        children.iter().all(|child_hash| {
            self.nodes.get(child_hash).is_some_and(|n| {
                matches!(
                    n.status(),
                    DerivationStatus::Completed | DerivationStatus::Skipped
                )
            })
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
                    DerivationStatus::Poisoned
                        | DerivationStatus::DependencyFailed
                        | DerivationStatus::Cancelled
                )
            })
        })
    }

    /// Get all parent drv_hashes that depend on the given child.
    pub fn get_parents(&self, child_hash: &str) -> Vec<DrvHash> {
        self.parents
            .get(child_hash)
            .map(|p| p.iter().cloned().collect())
            .unwrap_or_default()
    }

    /// Get all child drv_hashes that the given parent depends on.
    ///
    /// Mirror of `get_parents`. Critical-path computation walks DOWN
    /// (children), completion handling walks UP (parents). Both
    /// directions needed.
    pub fn get_children(&self, parent_hash: &str) -> Vec<DrvHash> {
        self.children
            .get(parent_hash)
            .map(|c| c.iter().cloned().collect())
            .unwrap_or_default()
    }

    /// Find all derivations that become ready (all deps completed) after a
    /// given derivation completes. Only returns derivations currently in Queued state.
    pub fn find_newly_ready(&self, completed_hash: &str) -> Vec<DrvHash> {
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

    // r[impl sched.ca.cutoff-propagate]
    /// Walk downstream from a CA-unchanged completion. Return derivations
    /// whose ONLY remaining incomplete dependency was the just-completed
    /// (or just-skipped) node — i.e., status is `Queued` and all deps are
    /// now terminal.
    ///
    /// Treats nodes in `provisional_skipped` as-if-terminal. Used by
    /// `speculative_cascade_reachable` (batch-verification prewalk +
    /// cascade transition walk): verification needs to speculate "if B
    /// WERE skipped, would C be eligible?" to batch the store RPC
    /// across all reachable candidates.
    ///
    /// Only `Queued` is checked (never touch `Running` —
    /// `r[sched.preempt.never-running]`). A `Ready` node has
    /// `all_deps_completed() == true`, which means its inputs were
    /// already fully built — nothing for CA cutoff to skip there, so
    /// excluding Ready is correct.
    ///
    /// The `is_terminal()` check technically accepts
    /// `Poisoned`/`DependencyFailed`/`Cancelled` deps, but in the
    /// single-threaded actor those states cascade DependencyFailed
    /// immediately (see `cascade_dependency_failure`), so a `Queued`
    /// node's terminal deps are in practice only `Completed` or
    /// `Skipped`.
    pub fn find_cutoff_eligible_speculative(
        &self,
        completed: &str,
        provisional_skipped: &HashSet<DrvHash>,
    ) -> Vec<DrvHash> {
        let mut eligible = Vec::new();
        for parent_hash in self.get_parents(completed) {
            let Some(state) = self.nodes.get(&parent_hash) else {
                continue;
            };
            // Queued only — never Running. Provisional-skipped nodes
            // are excluded too (they're being speculated AS skipped,
            // so they're "parents of" targets, not targets themselves).
            if state.status() != DerivationStatus::Queued
                || provisional_skipped.contains(&parent_hash)
            {
                continue;
            }
            // Eligible iff ALL deps now terminal (or provisionally
            // skipped). Other incomplete deps → not eligible.
            let all_terminal = self
                .children
                .get(&parent_hash)
                .map(|deps| {
                    deps.iter().all(|d| {
                        provisional_skipped.contains(d)
                            || self.nodes.get(d).is_some_and(|n| n.status().is_terminal())
                    })
                })
                .unwrap_or(true);
            if all_terminal {
                eligible.push(parent_hash);
            }
        }
        eligible
    }

    /// Generic BFS walk collecting all nodes reachable via
    /// `expand(current, visited)` from `trigger`, capped at `max_nodes`
    /// pops. Pure, non-mutating — the caller decides what to do with
    /// the result (transition, store-verify, persist).
    ///
    /// Used by:
    /// - [`Self::cascade_cutoff`] — Queued→Skipped propagation
    /// - [`crate::actor::DagActor`]'s `verify_cutoff_candidates` —
    ///   over-approximate candidate collection for a batched
    ///   FindMissingPaths RPC
    ///
    /// The `expand` closure receives `(current, &visited_so_far)` so
    /// it can implement speculative-skipped semantics (see
    /// [`Self::find_cutoff_eligible_speculative`] — treats
    /// provisional-visited nodes as already-Skipped).
    ///
    /// Returns `(reachable, cap_hit)`. Deduplication via `visited`
    /// HashSet — diamond DAGs are safe.
    ///
    /// Associated fn (not `&self`) to avoid overlapping borrows when
    /// `expand` needs to call `&self` methods while the caller is
    /// `&mut self` (see [`Self::cascade_cutoff`]).
    pub fn speculative_cascade_reachable<F>(
        trigger: &DrvHash,
        max_nodes: usize,
        mut expand: F,
    ) -> (Vec<DrvHash>, bool)
    where
        F: FnMut(&DrvHash, &HashSet<DrvHash>) -> Vec<DrvHash>,
    {
        let mut reachable = Vec::new();
        let mut visited: HashSet<DrvHash> = HashSet::new();
        let mut frontier = vec![trigger.clone()];
        let mut pops = 0usize;
        let mut cap_hit = false;
        while let Some(current) = frontier.pop() {
            if pops >= max_nodes {
                cap_hit = true;
                break;
            }
            for next in expand(&current, &visited) {
                if visited.insert(next.clone()) {
                    reachable.push(next.clone());
                    frontier.push(next);
                }
            }
            pops += 1;
        }
        (reachable, cap_hit)
    }

    // r[impl sched.ca.cutoff-propagate]
    /// Cascade CA-cutoff Skip transitions starting from a trigger node.
    ///
    /// Two-phase: (1) BFS-collect all verified-eligible downstream
    /// nodes via [`Self::speculative_cascade_reachable`], passing the
    /// visited set to [`Self::find_cutoff_eligible_speculative`] so
    /// already-collected nodes are treated as-if-Skipped for
    /// transitive eligibility (A unchanged → B would-skip → C depended
    /// only on B → C eligible). (2) Bulk-transition the collected set
    /// to `Skipped`. The `&mut self.nodes` second pass happens AFTER
    /// the `&self` walk returns — no borrow overlap.
    ///
    /// The `verify` closure gates against the self-match hazard
    /// (bughunt-mc196): `ca_output_unchanged` can be `true` for a
    /// FIRST-EVER build because PutPath inserts the content_index row
    /// before BuildComplete arrives, so ContentLookup matches the
    /// just-uploaded output. Without verification, downstream nodes
    /// would be Skipped even though their outputs have NEVER been
    /// built. The completion handler passes a closure that checks
    /// `expected_output_paths` exist in the store; tests pass `|_| true`
    /// for pure walk testing. Unverified nodes are filtered out of
    /// `expand`'s return → not added to visited → not treated
    /// as-if-Skipped → their parents stay ineligible (cascade halts).
    ///
    /// Depth-capped at [`MAX_CASCADE_NODES`]. Returns
    /// `(skipped_hashes, depth_cap_hit)`.
    pub fn cascade_cutoff(
        &mut self,
        trigger: &str,
        mut verify: impl FnMut(&DrvHash) -> bool,
    ) -> (Vec<DrvHash>, bool) {
        let Some(start) = self.canonical(trigger) else {
            return (Vec::new(), false);
        };
        let (candidates, cap_hit) =
            Self::speculative_cascade_reachable(&start, MAX_CASCADE_NODES, |current, visited| {
                self.find_cutoff_eligible_speculative(current, visited)
                    .into_iter()
                    .filter(&mut verify)
                    .collect()
            });
        let mut skipped = Vec::with_capacity(candidates.len());
        for hash in candidates {
            // Queued→Skipped. All candidates were Queued when
            // collected (find_cutoff_eligible_speculative only
            // returns Queued nodes); the HashSet dedup in the
            // walker guarantees each hash appears once.
            if let Some(state) = self.nodes.get_mut(&hash)
                && state.transition(DerivationStatus::Skipped).is_ok()
            {
                skipped.push(hash);
            }
        }
        (skipped, cap_hit)
    }

    /// Get all derivation hashes involved in a build.
    pub fn build_derivations(&self, build_id: Uuid) -> Vec<DrvHash> {
        self.nodes
            .iter()
            .filter(|(_, state)| state.interested_builds.contains(&build_id))
            .map(|(hash, _)| hash.clone())
            .collect()
    }

    /// Remove a build's interest from all its derivations.
    /// Returns derivation hashes that are now orphaned (no builds interested).
    pub fn remove_build_interest(&mut self, build_id: Uuid) -> Vec<DrvHash> {
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
            // HashSet::remove returns true iff the element was present.
            // Only reap nodes that THIS call emptied. Recovered-poisoned
            // nodes have interested_builds=∅ from birth (from_poisoned_row
            // at state/derivation.rs) — without this guard, the FIRST
            // build completion post-recovery reaps every one of them,
            // silently disabling poison-TTL tracking.
            let was_interested = state.interested_builds.remove(&build_id);
            if was_interested && state.interested_builds.is_empty() && state.status().is_terminal()
            {
                to_reap.push(hash.clone());
            }
        }

        let reaped = to_reap.len();
        for hash in to_reap {
            self.remove_node(&hash);
        }

        reaped
    }

    /// Remove a single node and scrub all edge references to it.
    ///
    /// Used by poison-clear paths (admin ClearPoison, TTL expiry) so the
    /// next merge treats the derivation as newly-inserted: it receives full
    /// proto fields and flows through `compute_initial_states`. Resetting
    /// status in-place instead would leave stub fields from
    /// `from_poisoned_row` (empty `output_names`, empty
    /// `expected_output_paths`) and `compute_initial_states` only iterates
    /// `newly_inserted` — the node would sit in Created forever.
    pub fn remove_node(&mut self, hash: &DrvHash) {
        if let Some(state) = self.nodes.remove(hash) {
            self.path_to_hash.remove(state.drv_path().as_str());
        }
        self.children.remove(hash);
        self.parents.remove(hash);
        // Also scrub this hash from other nodes' edge sets.
        for children in self.children.values_mut() {
            children.remove(hash);
        }
        for parents in self.parents.values_mut() {
            parents.remove(hash);
        }
    }

    /// Determine initial states for newly merged derivations.
    ///
    /// Derivations with no incomplete dependencies go to Queued, then
    /// immediately to Ready. Others stay in Created until they become Queued
    /// (when their dependencies complete).
    /// Derivations whose dependency is already Poisoned/DependencyFailed go
    /// directly to DependencyFailed (pre-poisoned detection).
    ///
    /// Returns lists of (drv_hash, new_status) transitions.
    pub fn compute_initial_states(
        &self,
        newly_inserted: &HashSet<DrvHash>,
    ) -> Vec<(DrvHash, DerivationStatus)> {
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
    // r[impl sched.dag.build-scoped-roots]
    /// Find all root derivations for a build.
    /// These are the top-level derivations the client actually wants built.
    ///
    /// A derivation is a root FOR THIS BUILD if no parent *interested
    /// in this build* depends on it. The global `parents` map includes
    /// parents from all merged builds — a derivation that's a root for
    /// build X may have a parent from build Y. Using the unscoped
    /// parent set incorrectly marks X's root as a non-root, stalling
    /// X's dispatch (bug_022).
    pub fn find_roots(&self, build_id: Uuid) -> Vec<DrvHash> {
        self.nodes
            .iter()
            .filter(|(_, state)| state.interested_builds.contains(&build_id))
            .filter(|(hash, _)| {
                // No parent interested in THIS build depends on it.
                !self.parents.get(hash.as_str()).is_some_and(|parents| {
                    parents.iter().any(|p| {
                        self.nodes
                            .get(p)
                            .is_some_and(|ps| ps.interested_builds.contains(&build_id))
                    })
                })
            })
            .map(|(hash, _)| hash.clone())
            .collect()
    }

    /// Compute summary counts for a build.
    ///
    /// Single pass over all DAG nodes. Besides the status-bucket
    /// counts, also collects:
    /// - `critpath_remaining`: max priority across non-terminal nodes.
    ///   Priority = est_duration + max(children priority), so the
    ///   build's root(s) hold the critical-path ETA. Taking the max
    ///   over ALL non-terminal nodes (not just `find_roots`) is
    ///   correct AND more robust: if X has a build-interested
    ///   parent P, then P.priority ≥ X.priority (P includes X in
    ///   its max-child), so the max is achieved at a node with no
    ///   build-interested parent anyway. This sidesteps the
    ///   `find_roots` global-vs-build-scoped-parent question.
    /// - `assigned_executors`: deduplicated WorkerIds with an
    ///   Assigned/Running derivation in this build. BTreeSet for
    ///   sorted iteration → deterministic proto wire order.
    pub fn build_summary(&self, build_id: Uuid) -> BuildSummary {
        let mut summary = BuildSummary::default();
        let mut workers: BTreeSet<String> = BTreeSet::new();

        for state in self.nodes.values() {
            if !state.interested_builds.contains(&build_id) {
                continue;
            }
            summary.total += 1;
            match state.status() {
                // Skipped counts as completed: output-equivalent (CA
                // cutoff means it would've produced byte-identical
                // output to what's already in the store). Build
                // accounting (check_build_completion) sees it as done.
                DerivationStatus::Completed | DerivationStatus::Skipped => summary.completed += 1,
                DerivationStatus::Running | DerivationStatus::Assigned => {
                    summary.running += 1;
                    // assigned_executor is Some exactly in these two
                    // states (cleared on every terminal transition +
                    // reset_to_ready). Defensive if_let anyway.
                    if let Some(w) = &state.assigned_executor {
                        workers.insert(w.to_string());
                    }
                }
                DerivationStatus::Failed
                | DerivationStatus::Poisoned
                | DerivationStatus::DependencyFailed
                // Cancelled counts as failed from the build-summary
                // perspective: the build didn't produce its output.
                // Distinct terminal reason but same "not done" bucket
                // for the total/completed/failed accounting the gateway
                // shows.
                | DerivationStatus::Cancelled => summary.failed += 1,
                DerivationStatus::Ready | DerivationStatus::Queued | DerivationStatus::Created => {
                    summary.queued += 1;
                }
            }
            // Critical-path: max priority across non-terminal. Terminal
            // nodes keep their stale priority (only ancestors are
            // recomputed on completion), so including them would
            // over-report.
            if !state.status().is_terminal() {
                summary.critpath_remaining = summary.critpath_remaining.max(state.priority);
            }
        }

        summary.assigned_executors = workers.into_iter().collect();
        summary
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
    /// Max `priority` across non-terminal nodes — the critical-path
    /// ETA in seconds. 0.0 when nothing's left (all terminal).
    pub critpath_remaining: f64,
    /// Deduplicated, sorted worker IDs currently assigned/running
    /// derivations in this build.
    pub assigned_executors: Vec<String>,
}

#[cfg(test)]
mod tests;
