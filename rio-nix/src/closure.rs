//! Cycle-safe reference-closure traversal primitives.
//!
//! Store-path reference graphs in rio can contain cycles: rio-store
//! deliberately admits cyclic reference metadata so that GC can reclaim
//! mutually-referencing garbage (`store.gc.sweep-cycle-reclaim`), unlike
//! CppNix's local store, whose `registerValidPaths` topological sort makes
//! cycles unrepresentable. That difference moves the cycle-safety
//! obligation from the store onto every consumer that walks reference
//! metadata — and a hand-rolled walker that forgets is a builder hang (or
//! unbounded memory growth) on adversary-supplied metadata.
//!
//! These primitives own that obligation, so consumers never write their
//! own graph loops:
//!
//! - [`ClosureSet::extend`]: incremental visited-set BFS (the
//!   `computeFSClosure` shape from CppNix `libstore/misc.cc`) — each node
//!   is resolved exactly once, so cycles cannot cause re-expansion or
//!   non-termination, and a node's resolution is never revisited by later
//!   extensions (snapshot semantics).
//! - [`find_cycle`]: Kahn-style peeling over a closed member set —
//!   returns the (sorted) cycle members plus the members on paths
//!   connecting two cycles; empty iff the member subgraph is acyclic.
//!   Self-references are ignored (a store path referencing itself is
//!   ordinary metadata).
//! - [`closure_sizes`]: per-member closure sizes with one reusable
//!   scratch set — auxiliary memory stays O(largest single closure),
//!   never O(members × closure size) like a memoized-set approach.
// r[impl nix.closure.cycle-safe]

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet, VecDeque};

/// A reference closure under incremental construction.
///
/// Members are `&str` borrowed from the caller's metadata (store-path
/// strings); the set never allocates copies of them.
#[derive(Debug, Default)]
pub struct ClosureSet<'a> {
    members: BTreeSet<&'a str>,
}

impl<'a> ClosureSet<'a> {
    /// An empty closure.
    pub fn new() -> Self {
        Self::default()
    }

    /// Expand the closure from `roots`, resolving each newly-reached
    /// node's references with `resolve`.
    ///
    /// `resolve` is called exactly once per node, the first time it is
    /// reached — nodes already in the set (whether from this call or a
    /// previous `extend`) are never re-resolved. That single property is
    /// what makes the walk cycle-safe (a cycle revisits a member, which
    /// is skipped) and gives later extensions snapshot semantics (they
    /// cannot change the resolution of an existing member).
    ///
    /// A resolver error aborts the walk and propagates; the set keeps the
    /// members reached before the error (callers that need atomicity
    /// discard the set on error).
    pub fn extend<E, I>(
        &mut self,
        roots: impl IntoIterator<Item = &'a str>,
        mut resolve: impl FnMut(&'a str) -> Result<I, E>,
    ) -> Result<(), E>
    where
        I: IntoIterator<Item = &'a str>,
    {
        let mut queue: VecDeque<&'a str> = roots.into_iter().collect();
        while let Some(path) = queue.pop_front() {
            if !self.members.insert(path) {
                continue;
            }
            for reference in resolve(path)? {
                if !self.members.contains(reference) {
                    queue.push_back(reference);
                }
            }
        }
        Ok(())
    }

    /// Iterate the members in sorted (lexicographic store-path) order —
    /// the order CppNix's `StorePathSet` iterates.
    pub fn members(&self) -> impl Iterator<Item = &'a str> + '_ {
        self.members.iter().copied()
    }

    /// Whether `path` is in the closure.
    pub fn contains(&self, path: &str) -> bool {
        self.members.contains(path)
    }

    /// Number of members.
    pub fn len(&self) -> usize {
        self.members.len()
    }

    /// Whether the closure is empty.
    pub fn is_empty(&self) -> bool {
        self.members.is_empty()
    }
}

/// Detect reference cycles within a closed member set.
///
/// `refs_of` resolves each member's references. References that are not
/// themselves members, and self-references, are ignored. Returns the
/// sorted set of members that participate in reference cycles, plus any
/// members lying on a path connecting two cycles (see below) — empty iff
/// the member subgraph is acyclic.
///
/// The detection is Kahn-style peeling, run in both directions: first
/// every member whose (in-set, non-self) references are all peeled is
/// removed (members that do not *reach* a cycle), then every remaining
/// member that no remaining member references is removed (members not
/// *reachable from* a cycle). What survives both passes participates in a
/// cycle, or lies on a path connecting two cycles. Linear in members +
/// edges; never recurses.
pub fn find_cycle<'a, I>(
    members: impl IntoIterator<Item = &'a str>,
    mut refs_of: impl FnMut(&'a str) -> I,
) -> Vec<&'a str>
where
    I: IntoIterator<Item = &'a str>,
{
    let member_set: BTreeSet<&'a str> = members.into_iter().collect();

    // In-set, non-self adjacency (forward = references; reverse =
    // referrers), plus the out-degree counter pass 1 peels against.
    let mut forward: HashMap<&'a str, Vec<&'a str>> = HashMap::with_capacity(member_set.len());
    let mut referrers: HashMap<&'a str, Vec<&'a str>> = HashMap::with_capacity(member_set.len());
    let mut out_degree: HashMap<&'a str, usize> = HashMap::with_capacity(member_set.len());
    for &path in &member_set {
        let refs: Vec<&'a str> = refs_of(path)
            .into_iter()
            .filter(|r| *r != path && member_set.contains(r))
            .collect();
        out_degree.insert(path, refs.len());
        for &reference in &refs {
            referrers.entry(reference).or_default().push(path);
        }
        forward.insert(path, refs);
    }

    // Pass 1: peel members whose references are all peeled ("leaves"
    // first). Survivors = members that transitively reach a cycle.
    let mut peeled: HashSet<&'a str> = HashSet::with_capacity(member_set.len());
    let mut queue: VecDeque<&'a str> = member_set
        .iter()
        .copied()
        .filter(|p| out_degree[p] == 0)
        .collect();
    while let Some(path) = queue.pop_front() {
        peeled.insert(path);
        for &referrer in referrers.get(path).map(Vec::as_slice).unwrap_or(&[]) {
            let degree = out_degree
                .get_mut(referrer)
                .expect("referrer is a member with a recorded out-degree");
            *degree -= 1;
            if *degree == 0 {
                queue.push_back(referrer);
            }
        }
    }

    // Pass 2 over the survivors: peel members no surviving member
    // references ("roots" first). Survivors of both passes participate
    // in (or connect) cycles.
    let survivors: BTreeSet<&'a str> = member_set
        .iter()
        .copied()
        .filter(|p| !peeled.contains(p))
        .collect();
    let mut in_degree: HashMap<&'a str, usize> = survivors.iter().map(|&p| (p, 0)).collect();
    for &path in &survivors {
        for &reference in &forward[path] {
            if let Some(d) = in_degree.get_mut(reference) {
                *d += 1;
            }
        }
    }
    let mut queue: VecDeque<&'a str> = survivors
        .iter()
        .copied()
        .filter(|p| in_degree[p] == 0)
        .collect();
    let mut emitted: HashSet<&'a str> = HashSet::with_capacity(survivors.len());
    while let Some(path) = queue.pop_front() {
        emitted.insert(path);
        for &reference in &forward[path] {
            if let Some(d) = in_degree.get_mut(reference) {
                *d -= 1;
                if *d == 0 {
                    queue.push_back(reference);
                }
            }
        }
    }

    survivors
        .into_iter()
        .filter(|p| !emitted.contains(p))
        .collect()
}

/// Per-member closure sizes over a closed member set.
///
/// For each member: the sum of `size_of` over its reference closure
/// (itself plus everything transitively reachable through `refs_of`),
/// each reachable node counted exactly once. References outside the
/// member set are still walked — callers decide their size and references
/// (typically `0` / no references for unknown paths, making them leaves).
///
/// Cycle-safe: each per-member walk uses a visited set, so cyclic
/// references terminate with every reachable node counted once. One
/// scratch set and one queue are reused across members, keeping auxiliary
/// memory at O(largest single closure) rather than O(members × closure
/// size) — the same shape as CppNix computing `closureSize` per path
/// (`parsed-derivations.cc` `getStructuredAttrs` exportReferencesGraph
/// handling) rather than memoizing every path's closure set.
pub fn closure_sizes<'a, I>(
    members: impl IntoIterator<Item = &'a str>,
    mut refs_of: impl FnMut(&'a str) -> I,
    mut size_of: impl FnMut(&'a str) -> u64,
) -> BTreeMap<&'a str, u64>
where
    I: IntoIterator<Item = &'a str>,
{
    let mut sizes: BTreeMap<&'a str, u64> = BTreeMap::new();
    // One scratch visited-set + queue, reused (cleared) per member.
    let mut visited: HashSet<&'a str> = HashSet::new();
    let mut queue: Vec<&'a str> = Vec::new();

    for member in members {
        visited.clear();
        queue.clear();
        queue.push(member);
        let mut total: u64 = 0;
        while let Some(path) = queue.pop() {
            if !visited.insert(path) {
                continue;
            }
            total = total.saturating_add(size_of(path));
            for reference in refs_of(path) {
                if !visited.contains(reference) {
                    queue.push(reference);
                }
            }
        }
        sizes.insert(member, total);
    }
    sizes
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;
    use std::convert::Infallible;

    /// Static adjacency map; unknown keys resolve to no references.
    type Graph = HashMap<&'static str, Vec<&'static str>>;

    fn graph(edges: &[(&'static str, &[&'static str])]) -> Graph {
        edges.iter().map(|(k, v)| (*k, v.to_vec())).collect()
    }

    fn closure_from(g: &Graph, roots: &[&'static str]) -> ClosureSet<'static> {
        let mut set = ClosureSet::new();
        set.extend(roots.iter().copied(), |p| {
            Ok::<_, Infallible>(g.get(p).cloned().unwrap_or_default())
        })
        .unwrap();
        set
    }

    fn members_of(set: &ClosureSet<'static>) -> Vec<&'static str> {
        set.members().collect()
    }

    #[test]
    fn transitive_closure_collected() {
        let g = graph(&[("a", &["b"]), ("b", &["c"]), ("c", &[])]);
        let set = closure_from(&g, &["a"]);
        assert_eq!(members_of(&set), vec!["a", "b", "c"]);
        assert!(set.contains("b"));
        assert!(!set.contains("z"));
        assert_eq!(set.len(), 3);
        assert!(!set.is_empty());
    }

    /// The core cycle-safety property: a two-cycle terminates instead of
    /// looping forever. On a regression this test hangs, which nextest's
    /// per-test timeout converts into a deterministic CI failure.
    // r[verify nix.closure.cycle-safe]
    #[test]
    fn extend_terminates_on_two_cycle() {
        let g = graph(&[("a", &["b"]), ("b", &["a"])]);
        let set = closure_from(&g, &["a"]);
        assert_eq!(members_of(&set), vec!["a", "b"]);
    }

    #[test]
    fn self_reference_is_tolerated() {
        let g = graph(&[("a", &["a"])]);
        let set = closure_from(&g, &["a"]);
        assert_eq!(members_of(&set), vec!["a"]);
    }

    /// Snapshot semantics: each node is resolved exactly once, even when
    /// reached again via a later root or a second `extend` call.
    #[test]
    fn members_are_not_reresolved() {
        let g = graph(&[("a", &["b"]), ("b", &[]), ("c", &["a"])]);
        let calls: RefCell<Vec<&'static str>> = RefCell::new(Vec::new());
        let mut set = ClosureSet::new();
        let mut resolve = |p: &'static str| {
            calls.borrow_mut().push(p);
            Ok::<_, Infallible>(g.get(p).cloned().unwrap_or_default())
        };
        set.extend(["a"], &mut resolve).unwrap();
        // "c" reaches "a" again; "a" and "b" must not be re-resolved.
        set.extend(["c", "a"], &mut resolve).unwrap();
        assert_eq!(members_of(&set), vec!["a", "b", "c"]);
        let mut counts: HashMap<&str, usize> = HashMap::new();
        for p in calls.borrow().iter() {
            *counts.entry(p).or_default() += 1;
        }
        assert_eq!(counts["a"], 1, "a resolved once across both extends");
        assert_eq!(counts["b"], 1);
        assert_eq!(counts["c"], 1);
    }

    #[test]
    fn extend_is_incremental() {
        let g = graph(&[("a", &["b"]), ("b", &[]), ("d", &["e"]), ("e", &[])]);
        let mut set = closure_from(&g, &["a"]);
        set.extend(["d"], |p| {
            Ok::<_, Infallible>(g.get(p).cloned().unwrap_or_default())
        })
        .unwrap();
        assert_eq!(members_of(&set), vec!["a", "b", "d", "e"]);
    }

    #[test]
    fn resolver_error_propagates() {
        let g = graph(&[("a", &["boom"]), ("boom", &[])]);
        let mut set = ClosureSet::new();
        let err = set
            .extend(["a"], |p| {
                if p == "boom" {
                    Err("no metadata for boom")
                } else {
                    Ok(g.get(p).cloned().unwrap_or_default())
                }
            })
            .unwrap_err();
        assert_eq!(err, "no metadata for boom");
        // The members reached before the error are retained.
        assert!(set.contains("a"));
    }

    /// A reference to a path the resolver knows nothing about is a leaf,
    /// not an error — the resolver decides (here: empty references).
    #[test]
    fn unknown_refs_are_leaves() {
        let g = graph(&[("a", &["mystery"])]);
        let set = closure_from(&g, &["a"]);
        assert_eq!(members_of(&set), vec!["a", "mystery"]);
    }

    /// Diamonds (shared dependencies) are NOT cycles — the classic
    /// false-positive trap for naive cycle detectors.
    #[test]
    fn diamond_is_not_a_cycle() {
        let g = graph(&[("a", &["b", "c"]), ("b", &["d"]), ("c", &["d"]), ("d", &[])]);
        let set = closure_from(&g, &["a"]);
        let cyclic = find_cycle(set.members(), |p| g.get(p).cloned().unwrap_or_default());
        assert!(
            cyclic.is_empty(),
            "diamond misreported as cycle: {cyclic:?}"
        );
    }

    /// Cycle members are reported (sorted); members that merely reference
    /// into the cycle are not.
    // r[verify nix.closure.cycle-safe]
    #[test]
    fn find_cycle_reports_cycle_members() {
        let g = graph(&[("a", &["b"]), ("b", &["c"]), ("c", &["b"])]);
        let set = closure_from(&g, &["a"]);
        let cyclic = find_cycle(set.members(), |p| g.get(p).cloned().unwrap_or_default());
        assert_eq!(cyclic, vec!["b", "c"]);
    }

    #[test]
    fn find_cycle_ignores_self_references() {
        let g = graph(&[("a", &["a", "b"]), ("b", &["b"])]);
        let set = closure_from(&g, &["a"]);
        let cyclic = find_cycle(set.members(), |p| g.get(p).cloned().unwrap_or_default());
        assert!(
            cyclic.is_empty(),
            "self-references are not cycles: {cyclic:?}"
        );
    }

    /// The 70/60/40 fixture mirrored from the builder's
    /// `exportReferencesGraph` tests: shared dependencies are counted
    /// once per member closure.
    #[test]
    fn closure_sizes_count_shared_deps_once() {
        let g = graph(&[("a", &["b", "c"]), ("b", &["c"]), ("c", &["c"])]);
        let sizes_by: HashMap<&str, u64> = [("a", 10), ("b", 20), ("c", 40)].into();
        let set = closure_from(&g, &["a"]);
        let sizes = closure_sizes(
            set.members(),
            |p| g.get(p).cloned().unwrap_or_default(),
            |p| sizes_by[p],
        );
        assert_eq!(sizes["a"], 70);
        assert_eq!(sizes["b"], 60);
        assert_eq!(sizes["c"], 40);
    }

    /// Sizing a cyclic closure terminates, with every cycle member
    /// counted exactly once.
    // r[verify nix.closure.cycle-safe]
    #[test]
    fn closure_sizes_terminate_on_cycle() {
        let g = graph(&[("a", &["b"]), ("b", &["a"])]);
        let set = closure_from(&g, &["a"]);
        let sizes = closure_sizes(
            set.members(),
            |p| g.get(p).cloned().unwrap_or_default(),
            |p| if p == "a" { 10 } else { 20 },
        );
        assert_eq!(sizes["a"], 30);
        assert_eq!(sizes["b"], 30);
    }

    /// Structural smoke test on a long chain: every node reached, sizes
    /// are the suffix sums. No wall-clock assertion (load-sensitive);
    /// the structural counts are the regression signal.
    #[test]
    fn thousand_node_chain() {
        let names: Vec<String> = (0..1000).map(|i| format!("n{i:04}")).collect();
        let refs: HashMap<&str, Vec<&str>> = names
            .iter()
            .enumerate()
            .map(|(i, n)| {
                let next: Vec<&str> = names.get(i + 1).map(|s| s.as_str()).into_iter().collect();
                (n.as_str(), next)
            })
            .collect();

        let mut set = ClosureSet::new();
        set.extend([names[0].as_str()], |p| {
            Ok::<_, Infallible>(refs.get(p).cloned().unwrap_or_default())
        })
        .unwrap();
        assert_eq!(set.len(), 1000);

        let cyclic = find_cycle(set.members(), |p| refs.get(p).cloned().unwrap_or_default());
        assert!(cyclic.is_empty());

        let sizes = closure_sizes(
            set.members(),
            |p| refs.get(p).cloned().unwrap_or_default(),
            |_| 1,
        );
        // Chain suffix sums: n0000 sees all 1000, the last sees 1.
        assert_eq!(sizes[names[0].as_str()], 1000);
        assert_eq!(sizes[names[999].as_str()], 1);
    }
}
