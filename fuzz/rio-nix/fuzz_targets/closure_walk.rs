#![no_main]

//! Fuzz the `rio_nix::closure` traversal primitives with adversarial
//! reference graphs.
//!
//! The properties under test are exactly the ones merged_bug_009 showed a
//! hand-rolled walker can violate:
//!
//! 1. **Termination** on arbitrary (including cyclic) graphs — libFuzzer's
//!    `-timeout=30` is the hang detector; a regression shows up as a
//!    timeout crash, not a silent slow test.
//! 2. **Closure containment**: every member reached from the roots is in
//!    the node universe; extending twice is a no-op (determinism).
//! 3. **Size monotonicity**: `r ∈ refs(p)` implies
//!    `closure_size(r) <= closure_size(p)` (a path's closure contains its
//!    references' closures), and every size is bounded by the sum of all
//!    node sizes.
//! 4. **Acyclic differential oracle**: when `find_cycle` reports no cycle,
//!    the sizes must equal a naive memoized-set computation (the shape the
//!    production code used before — correct on DAGs, broken on cycles).

use std::collections::{BTreeMap, BTreeSet};
use std::sync::OnceLock;

use libfuzzer_sys::fuzz_target;
use rio_nix::closure::{ClosureSet, closure_sizes, find_cycle};

/// Stable node-name table: fuzz inputs index into this instead of
/// allocating per-run names (keeps the fuzz process allocation-flat).
static NAMES: OnceLock<Vec<String>> = OnceLock::new();

fn names() -> &'static [String] {
    NAMES.get_or_init(|| (0..256).map(|i| format!("node-{i:03}")).collect())
}

/// Decode bytes into a directed graph over at most 256 nodes:
/// `input[0]` picks the node count (1..=256); the rest is consumed as
/// `(from, to)` edge pairs, both reduced mod the node count.
fn decode(input: &[u8]) -> (Vec<&'static str>, BTreeMap<&'static str, Vec<&'static str>>) {
    let table = names();
    let n = (input.first().copied().unwrap_or(0) as usize) + 1;
    let nodes: Vec<&'static str> = table[..n].iter().map(String::as_str).collect();

    let mut edges: BTreeMap<&'static str, Vec<&'static str>> = BTreeMap::new();
    for pair in input.get(1..).unwrap_or(&[]).chunks_exact(2) {
        let from = nodes[pair[0] as usize % n];
        let to = nodes[pair[1] as usize % n];
        edges.entry(from).or_default().push(to);
    }
    (nodes, edges)
}

/// Naive memoized closure-set computation — the differential oracle for
/// acyclic graphs (the pre-rio_nix::closure production algorithm: correct
/// on DAGs, divergent/non-terminating on cycles, which is why it is only
/// consulted when `find_cycle` reports none).
fn naive_acyclic_sizes(
    nodes: &[&'static str],
    edges: &BTreeMap<&'static str, Vec<&'static str>>,
    size_of: impl Fn(&str) -> u64,
) -> BTreeMap<&'static str, u64> {
    fn set_of<'g>(
        node: &'static str,
        edges: &'g BTreeMap<&'static str, Vec<&'static str>>,
        memo: &mut BTreeMap<&'static str, BTreeSet<&'static str>>,
        in_progress: &mut BTreeSet<&'static str>,
    ) -> BTreeSet<&'static str> {
        if let Some(s) = memo.get(node) {
            return s.clone();
        }
        // Cycle guard for the oracle itself (only consulted on acyclic
        // graphs, but the guard keeps the fuzzer honest if find_cycle
        // were to under-report).
        if !in_progress.insert(node) {
            return BTreeSet::from([node]);
        }
        let mut set = BTreeSet::from([node]);
        for r in edges.get(node).map(Vec::as_slice).unwrap_or(&[]) {
            if *r != node {
                set.extend(set_of(r, edges, memo, in_progress));
            }
        }
        in_progress.remove(node);
        memo.insert(node, set.clone());
        set
    }

    let mut memo = BTreeMap::new();
    let mut in_progress = BTreeSet::new();
    nodes
        .iter()
        .map(|n| {
            let set = set_of(n, edges, &mut memo, &mut in_progress);
            (*n, set.iter().map(|p| size_of(p)).sum())
        })
        .collect()
}

fuzz_target!(|input: &[u8]| {
    let (nodes, edges) = decode(input);
    let refs_of = |p: &str| -> Vec<&'static str> { edges.get(p).cloned().unwrap_or_default() };
    // Deterministic per-node sizes: index + 1.
    let size_of = |p: &str| -> u64 { (nodes.iter().position(|n| *n == p).unwrap_or(0) as u64) + 1 };

    // ── 1. ClosureSet::extend terminates and stays inside the universe.
    let mut set = ClosureSet::new();
    set.extend([nodes[0]], |p| {
        Ok::<_, std::convert::Infallible>(refs_of(p))
    })
    .unwrap();
    let members: Vec<&str> = set.members().collect();
    for m in &members {
        assert!(nodes.contains(m), "member {m} escaped the node universe");
    }

    // Determinism: extending again from the same root adds nothing.
    let len_before = set.len();
    set.extend([nodes[0]], |p| {
        Ok::<_, std::convert::Infallible>(refs_of(p))
    })
    .unwrap();
    assert_eq!(set.len(), len_before, "re-extend must be a no-op");

    // ── 2. find_cycle terminates; self-refs alone are never cycles.
    let cyclic = find_cycle(set.members(), refs_of);
    for c in &cyclic {
        assert!(members.contains(c), "cycle member {c} not in closure");
    }

    // ── 3. closure_sizes terminates; bounds + monotonicity hold.
    let sizes = closure_sizes(set.members(), refs_of, size_of);
    let universe_total: u64 = nodes.iter().map(|n| size_of(n)).sum();
    for (member, size) in &sizes {
        assert!(
            *size >= size_of(member),
            "size must include the member itself"
        );
        assert!(*size <= universe_total, "size exceeds the whole universe");
        for r in refs_of(member) {
            if let Some(ref_size) = sizes.get(r) {
                assert!(
                    ref_size <= size,
                    "monotonicity violated: size({r})={ref_size} > size({member})={size}"
                );
            }
        }
    }

    // ── 4. Acyclic differential oracle: on DAGs, sizes match the naive
    //       memoized-set reference computation.
    if cyclic.is_empty() {
        let oracle = naive_acyclic_sizes(&members, &edges, size_of);
        for (member, size) in &sizes {
            assert_eq!(
                size, &oracle[member],
                "acyclic size divergence at {member}: ours={size} oracle={}",
                oracle[member]
            );
        }
    }
});
