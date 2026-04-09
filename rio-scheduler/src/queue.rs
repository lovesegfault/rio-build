//! Priority ready queue for derivation dispatch.
//!
//! Replaces the phase-2a FIFO VecDeque. Derivations are popped in
//! critical-path priority order (highest first): dispatch the work
//! that unblocks the most downstream, not just the oldest.
//!
//! # BinaryHeap + lazy invalidation
//!
//! `BinaryHeap` can't do `remove(x)` in O(log n) — only pop. But we
//! NEED remove (cancellation, re-prioritization). The standard
//! workaround: keep a `members` set alongside the heap. `remove(x)`
//! takes x out of `members`; `pop()` skips heap entries whose hash is
//! no longer in `members`. Same amortized complexity, way simpler than
//! a hand-rolled indexed heap.
//!
//! Stale heap entries (removed or duplicate-push) accumulate until
//! `compact()` rebuilds the heap from `members`. Called on Tick when
//! garbage exceeds 50% of the heap — bounded memory, amortized O(1)
//! per operation.
//!
//! # Priority representation
//!
//! `(OrderedFloat<f64>, Reverse<u64>, DrvHash)`:
//! - `OrderedFloat`: f64 doesn't impl Ord (NaN). Wrapper fixes it.
//! - `Reverse<u64>`: FIFO tiebreak for equal priority. Without this,
//!   ties are broken by DrvHash (alphabetical), which is arbitrary.
//!   `Reverse` because lower sequence number = older = first; but
//!   BinaryHeap is max-heap, so Reverse makes lower sort higher.
//! - `DrvHash`: the actual payload.
//!
//! # Interactive boost
//!
//! Phase 2a used `push_front` for interactive (IFD) builds. That
//! doesn't translate to a heap. Instead: add a large constant to
//! interactive builds' priority. `INTERACTIVE_BOOST = 1e9` dwarfs
//! any realistic critical-path value (even a 10k-derivation DAG at
//! 300s each is only 3e6; a 1.2M-derivation DAG at 300s would be
//! 3.6e8 — 1e9 still wins).

use std::cmp::Reverse;
use std::collections::{BinaryHeap, HashSet};

use ordered_float::OrderedFloat;

use crate::state::DrvHash;

/// Priority boost for interactive (IFD) builds. Large enough to
/// dominate any realistic critical-path sum. A 100k-node DAG with
/// every node estimated at 1 hour is 3.6e8 total — 1e9 still wins.
///
/// Not `f64::MAX` because that would make ALL interactive builds
/// tie on priority (then break on sequence), losing the relative
/// critical-path ordering WITHIN interactive. We want interactive
/// builds to also be ordered among themselves.
pub const INTERACTIVE_BOOST: f64 = 1e9;

/// Entry in the BinaryHeap. Tuple so derived Ord works
/// (lexicographic: priority first, then reverse sequence, then hash).
type Entry = (OrderedFloat<f64>, Reverse<u64>, DrvHash);

/// Priority queue of ready derivations.
///
/// `push(hash, priority)` and `pop()` are O(log n). `remove(hash)`
/// is O(1) (just drops from `members`). `len()` is O(1) and reflects
/// live entries only.
#[derive(Debug)]
pub struct ReadyQueue {
    /// Max-heap: highest priority pops first.
    heap: BinaryHeap<Entry>,
    /// Monotonic counter for FIFO tiebreak. Wraps at u64::MAX —
    /// after 1.8e19 pushes, the tiebreak order flips once. Don't care.
    seq: u64,
    /// Hashes currently live in the queue. `pop()` skips heap entries
    /// whose hash is not here (removed or already-popped duplicate).
    /// Same HashSet-for-dedup pattern as the old FIFO.
    members: HashSet<DrvHash>,
}

impl ReadyQueue {
    pub fn new() -> Self {
        Self {
            heap: BinaryHeap::new(),
            seq: 0,
            members: HashSet::new(),
        }
    }

    /// Push a derivation with the given priority.
    ///
    /// If `hash` is already in the queue, this pushes a duplicate
    /// entry. `pop()` returns the higher-priority entry first and
    /// then skips the lower one (no longer in `members`). Net effect:
    /// re-push with **higher** priority takes effect; re-push with
    /// **lower-or-equal** priority is a no-op (the existing higher
    /// entry pops first; the duplicate is then skipped). Callers only
    /// ever raise priority — a new interested build raises a shared
    /// derivation's priority via the critical-path machinery and the
    /// re-push in `handle_merge_dag`.
    pub fn push(&mut self, hash: DrvHash, priority: f64) {
        // Already-present case: just push again. pop() sees two
        // entries for the same hash; the FIRST popped (higher
        // priority) wins and removes the hash from `members`; the
        // SECOND is later skipped because it's no longer in
        // `members`. The `members` check in pop() handles both dedup
        // and cancellation — no separate tombstone set needed.
        self.members.insert(hash.clone());
        self.seq = self.seq.wrapping_add(1);
        self.heap
            .push((OrderedFloat(priority), Reverse(self.seq), hash));
    }

    /// Pop the highest-priority derivation.
    ///
    /// Skips entries that were removed (lazy invalidation) or that
    /// are duplicate pushes of a hash already popped. Both reduce to
    /// "is this hash still in `members`?".
    pub fn pop(&mut self) -> Option<DrvHash> {
        loop {
            let (_, _, hash) = self.heap.pop()?;
            if self.members.remove(&hash) {
                return Some(hash);
            }
            // Not in members: stale entry (removed or duplicate). Skip.
        }
    }

    /// Mark a derivation as removed. Lazy: doesn't touch the heap.
    ///
    /// Returns `true` if it was in the queue, `false` otherwise
    /// (same signature as the old remove).
    pub fn remove(&mut self, hash: &str) -> bool {
        self.members.remove(hash)
    }

    /// Remove all entries. Used by recover_from_pg() to start
    /// fresh before reloading Ready derivations from PG.
    pub fn clear(&mut self) {
        self.heap.clear();
        self.members.clear();
        // seq_counter: DON'T reset. Monotonic-forever is fine
        // (it's just a FIFO tiebreak, wraps at u64::MAX after
        // ~500 billion years) and resetting would be a subtle
        // ordering bug if clear() were ever called mid-session
        // with entries re-pushed.
    }

    /// Number of valid (not-removed) entries.
    // `is_empty` pair was dead code — `pub(crate) mod queue` revealed it.
    #[allow(clippy::len_without_is_empty)]
    pub fn len(&self) -> usize {
        self.members.len()
    }

    /// Rebuild the heap without removed/stale entries.
    ///
    /// Called on Tick when garbage (heap.len() - members.len()) exceeds
    /// 50% of the heap. O(n log n) but amortized over many cheap
    /// remove()/push() calls. Without this, a long-running scheduler
    /// with lots of cancellations would leak heap memory unboundedly.
    pub fn compact(&mut self) {
        // Threshold: skip if garbage is <50% of heap. Compacting
        // every Tick when there's 1 stale entry is wasteful. Also
        // skip when the heap is empty — `0 < 0` is false, so without
        // this guard an empty queue would drain+rebuild every Tick.
        let garbage = self.heap.len().saturating_sub(self.members.len());
        if self.heap.is_empty() || garbage * 2 < self.heap.len() {
            return;
        }

        // Drain the heap, keep only entries in members. For
        // duplicates (same hash, multiple pushes), keep the entry
        // that would have popped first under the FULL Entry ordering
        // — highest priority, then lowest seq (FIFO), then hash.
        // `drain()` is arbitrary order so we max-reduce.
        let mut best: std::collections::HashMap<DrvHash, Entry> =
            std::collections::HashMap::with_capacity(self.members.len());
        for entry in self.heap.drain() {
            let hash = &entry.2;
            if !self.members.contains(hash) {
                continue; // removed or stale
            }
            best.entry(hash.clone())
                .and_modify(|existing| {
                    // Full-tuple compare: equal-priority duplicates
                    // resolve deterministically on Reverse<seq> (older
                    // wins), matching pop() semantics. Comparing only
                    // .0 left the survivor up to drain() order.
                    if entry > *existing {
                        *existing = entry.clone();
                    }
                })
                .or_insert(entry);
        }

        self.heap = best.into_values().collect();
    }
}

impl Default for ReadyQueue {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pops_in_priority_order() {
        let mut q = ReadyQueue::new();
        q.push("low".into(), 10.0);
        q.push("high".into(), 100.0);
        q.push("mid".into(), 50.0);

        assert_eq!(q.pop(), Some("high".into()));
        assert_eq!(q.pop(), Some("mid".into()));
        assert_eq!(q.pop(), Some("low".into()));
        assert_eq!(q.pop(), None);
    }

    #[test]
    fn fifo_tiebreak() {
        // Same priority → FIFO order (older sequence pops first).
        let mut q = ReadyQueue::new();
        q.push("first".into(), 50.0);
        q.push("second".into(), 50.0);
        q.push("third".into(), 50.0);

        assert_eq!(q.pop(), Some("first".into()));
        assert_eq!(q.pop(), Some("second".into()));
        assert_eq!(q.pop(), Some("third".into()));
    }

    #[test]
    fn remove_skips_on_pop() {
        let mut q = ReadyQueue::new();
        q.push("a".into(), 30.0);
        q.push("b".into(), 20.0);
        q.push("c".into(), 10.0);

        assert!(q.remove("b"));
        assert_eq!(q.len(), 2);

        assert_eq!(q.pop(), Some("a".into()));
        assert_eq!(q.pop(), Some("c".into())); // b skipped
        assert_eq!(q.pop(), None);
    }

    #[test]
    fn remove_nonexistent() {
        let mut q = ReadyQueue::new();
        assert!(!q.remove("not-there"));
    }

    #[test]
    fn repush_updates_priority() {
        // Push at low priority, re-push at high → should pop high first.
        let mut q = ReadyQueue::new();
        q.push("x".into(), 10.0);
        q.push("other".into(), 50.0);
        q.push("x".into(), 100.0); // re-push, higher

        // x at 100 should pop before other at 50.
        assert_eq!(q.pop(), Some("x".into()));
        assert_eq!(q.pop(), Some("other".into()));
        // The stale x@10 entry is still in the heap but x is no
        // longer in members → skipped.
        assert_eq!(q.pop(), None);
    }

    #[test]
    fn interactive_boost_dominates() {
        // A 100k-node chain at 3600s each has cumulative priority
        // 3.6e8. INTERACTIVE_BOOST (1e9) should still win.
        let mut q = ReadyQueue::new();
        q.push("huge-chain-root".into(), 3.6e8);
        q.push("interactive-leaf".into(), 5.0 + INTERACTIVE_BOOST);

        assert_eq!(q.pop(), Some("interactive-leaf".into()));
    }

    #[test]
    fn compact_threshold() {
        let mut q = ReadyQueue::new();
        for i in 0..100 {
            q.push(format!("item-{i}").into(), i as f64);
        }
        // Remove 40 (< 50% garbage). compact() should skip.
        for i in 0..40 {
            q.remove(&format!("item-{i}"));
        }
        let heap_before = q.heap.len();
        q.compact();
        assert_eq!(q.heap.len(), heap_before, "below threshold → no compact");

        // Remove 20 more (60 total, 60% garbage). compact() runs.
        for i in 40..60 {
            q.remove(&format!("item-{i}"));
        }
        q.compact();
        assert_eq!(q.heap.len(), 40, "above threshold → compacted to members");
        assert_eq!(q.heap.len(), q.members.len());
    }

    #[test]
    fn compact_empty_is_noop() {
        // Regression: `garbage * 2 < heap.len()` is `0 < 0 → false`
        // when empty, so an empty queue used to drain+rebuild every
        // Tick. The early-return guard makes it a true no-op.
        let mut q = ReadyQueue::new();
        q.compact();
        assert_eq!(q.heap.len(), 0);
        assert_eq!(q.len(), 0);
    }

    #[test]
    fn compact_preserves_pop_order() {
        let mut q = ReadyQueue::new();
        q.push("a".into(), 30.0);
        q.push("b".into(), 20.0);
        q.push("c".into(), 10.0);
        // Create garbage to trigger compact.
        for i in 0..10 {
            q.push(format!("junk-{i}").into(), 5.0);
            q.remove(&format!("junk-{i}"));
        }

        q.compact();

        // Pop order preserved.
        assert_eq!(q.pop(), Some("a".into()));
        assert_eq!(q.pop(), Some("b".into()));
        assert_eq!(q.pop(), Some("c".into()));
    }

    #[test]
    fn compact_keeps_highest_of_duplicates() {
        let mut q = ReadyQueue::new();
        q.push("x".into(), 10.0);
        q.push("x".into(), 100.0); // re-push, creates duplicate in heap
        // Add garbage to trigger compact.
        for i in 0..10 {
            q.push(format!("junk-{i}").into(), 5.0);
            q.remove(&format!("junk-{i}"));
        }

        q.compact();

        // After compact, only one x entry, and it's the high-priority one.
        assert_eq!(q.heap.len(), 1);
        assert_eq!(q.pop(), Some("x".into()));
        assert_eq!(q.pop(), None);
    }

    #[test]
    fn compact_preserves_fifo_on_equal_priority() {
        // Regression: with prio-only compare, the survivor among
        // equal-priority duplicates depended on drain() order, so
        // post-compact FIFO tiebreak vs OTHER equal-prio hashes was
        // nondeterministic. Full-Entry compare keeps the older seq.
        let mut q = ReadyQueue::new();
        q.push("a".into(), 50.0); // seq=1
        q.push("b".into(), 50.0); // seq=2
        q.push("a".into(), 50.0); // seq=3, equal-prio re-push
        // Garbage to trigger compact.
        for i in 0..10 {
            q.push(format!("junk-{i}").into(), 5.0);
            q.remove(&format!("junk-{i}"));
        }
        q.compact();
        // Surviving "a" must keep seq=1 (older), so "a" pops before "b".
        assert_eq!(q.pop(), Some("a".into()));
        assert_eq!(q.pop(), Some("b".into()));
    }

    /// Stress: 10k pushes/removes, invariants hold.
    #[test]
    fn stress_invariants() {
        let mut q = ReadyQueue::new();
        for i in 0..10_000 {
            q.push(format!("item-{i}").into(), (i % 100) as f64);
        }
        assert_eq!(q.len(), 10_000);

        for i in (0..10_000).step_by(2) {
            assert!(q.remove(&format!("item-{i}")));
        }
        assert_eq!(q.len(), 5_000);

        q.compact();
        assert_eq!(q.len(), 5_000);

        let mut popped = 0;
        let mut last_prio = f64::INFINITY;
        while let Some(hash) = q.pop() {
            // Verify: monotonically non-increasing priority.
            let i: u64 = hash
                .as_str()
                .strip_prefix("item-")
                .unwrap()
                .parse()
                .unwrap();
            let prio = (i % 100) as f64;
            assert!(prio <= last_prio, "pop order violated at {hash}");
            last_prio = prio;
            popped += 1;
        }
        assert_eq!(popped, 5_000);
    }
}
