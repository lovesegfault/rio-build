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
//! workaround: keep a `removed` set alongside the heap. `remove(x)`
//! just marks x in the set; `pop()` skips over marked entries. Same
//! amortized complexity, way simpler than a hand-rolled indexed heap.
//!
//! The `removed` set grows until `compact()` rebuilds the heap
//! without marked entries. Called on Tick when the garbage exceeds
//! 50% of the heap — bounded memory, amortized O(1) per operation.
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
/// is O(1) (just marks in a set). `len()` is O(1) and accounts for
/// the removed markers.
#[derive(Debug)]
pub struct ReadyQueue {
    /// Max-heap: highest priority pops first.
    heap: BinaryHeap<Entry>,
    /// Hashes marked as removed. `pop()` skips these. Cleared on
    /// `compact()`.
    removed: HashSet<DrvHash>,
    /// Monotonic counter for FIFO tiebreak. Wraps at u64::MAX —
    /// after 1.8e19 pushes, the tiebreak order flips once. Don't care.
    seq: u64,
    /// Members currently in the heap (not removed). For O(1) dedup
    /// in `push()` — same HashSet-for-dedup pattern as the old FIFO.
    members: HashSet<DrvHash>,
}

impl ReadyQueue {
    pub fn new() -> Self {
        Self {
            heap: BinaryHeap::new(),
            removed: HashSet::new(),
            seq: 0,
            members: HashSet::new(),
        }
    }

    /// Push a derivation with the given priority.
    ///
    /// If `hash` is already in the queue, this RE-PUSHES with the new
    /// priority. The old entry is lazily invalidated (removed set);
    /// `pop()` will skip it. This is how re-prioritization works
    /// (e.g., a new interested build raises a shared derivation's
    /// priority).
    ///
    /// Different from the old `push_back`/`push_front` dedup: that
    /// was "ignore if present". This is "update priority if present".
    /// The D4 critical-path machinery can change a node's priority
    /// after it's already queued.
    pub fn push(&mut self, hash: DrvHash, priority: f64) {
        if self.members.contains(&hash) {
            // Already present — mark the OLD entry as removed (we
            // can't find it in the heap to edit it). The new entry
            // goes in with the new priority. The old entry's hash is
            // in `removed`, so pop() skips it... but wait, the NEW
            // entry has the same hash. pop() would skip it too.
            //
            // Fix: don't add to removed; just push again. pop()
            // will see two entries for the same hash. The FIRST one
            // popped (higher priority) is returned; members.remove()
            // marks the hash as gone; the SECOND one pops later and
            // is skipped (not in members). This works because pop()
            // checks members, not removed, for the final gate.
            //
            // Actually, let me re-check the pop logic. It needs to
            // be: skip if (in removed) OR (not in members). Let me
            // simplify: just use members. removed is redundant if
            // we check members in pop.
        }
        // Insert returns true if newly added. Either way we push.
        self.members.insert(hash.clone());
        self.seq = self.seq.wrapping_add(1);
        self.heap
            .push((OrderedFloat(priority), Reverse(self.seq), hash));
    }

    /// Pop the highest-priority derivation.
    ///
    /// Skips entries that were removed (lazy invalidation) or that
    /// are duplicate pushes of a hash already popped.
    pub fn pop(&mut self) -> Option<DrvHash> {
        loop {
            let (_, _, hash) = self.heap.pop()?;
            // Two reasons to skip:
            // 1. Explicitly removed (cancellation).
            // 2. Duplicate push — a later push() with the same hash
            //    went into the heap; an earlier pop already returned
            //    it. Members was removed then; this stale entry skips.
            //
            // Both reduce to: "is this hash still in members?" If
            // yes, pop it (remove from members, return). If no, skip.
            if self.members.remove(&hash) {
                // Was in members → valid pop. Also remove from
                // removed (in case it was there from a remove() call
                // that was superseded by a re-push... actually no,
                // remove() takes it out of members too. So if it's
                // in members, it's not in removed. Still, remove()
                // on a non-present key is a cheap no-op.)
                self.removed.remove(&hash);
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
        // Take out of members (so pop skips it) + add to removed
        // (for compact() to know it's garbage).
        if self.members.remove(hash) {
            self.removed.insert(hash.into());
            true
        } else {
            false
        }
    }

    /// Number of valid (not-removed) entries.
    // `is_empty` pair was dead code — `pub(crate) mod queue` revealed it.
    #[allow(clippy::len_without_is_empty)]
    /// Remove all entries. Used by recover_from_pg() to start
    /// fresh before reloading Ready derivations from PG.
    pub fn clear(&mut self) {
        self.heap.clear();
        self.members.clear();
        self.removed.clear();
        // seq_counter: DON'T reset. Monotonic-forever is fine
        // (it's just a FIFO tiebreak, wraps at u64::MAX after
        // ~500 billion years) and resetting would be a subtle
        // ordering bug if clear() were ever called mid-session
        // with entries re-pushed.
    }

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
        // every Tick when there's 1 stale entry is wasteful.
        let garbage = self.heap.len().saturating_sub(self.members.len());
        if garbage * 2 < self.heap.len() {
            return;
        }

        // Drain the heap, keep only entries that are in members.
        // For duplicates (same hash, multiple pushes), keep only
        // the FIRST occurrence (highest priority — drain is in
        // arbitrary order but we track seen hashes).
        //
        // Actually drain() is arbitrary order, so "first occurrence"
        // isn't "highest priority". We need to keep the HIGHEST
        // priority entry for each hash. Collect by hash, max by
        // priority.
        let mut best: std::collections::HashMap<DrvHash, Entry> =
            std::collections::HashMap::with_capacity(self.members.len());
        for entry in self.heap.drain() {
            let (prio, _, ref hash) = entry;
            if !self.members.contains(hash) {
                continue; // removed or stale
            }
            best.entry(hash.clone())
                .and_modify(|existing| {
                    if prio > existing.0 {
                        *existing = entry.clone();
                    }
                })
                .or_insert(entry);
        }

        self.heap = best.into_values().collect();
        self.removed.clear();
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
        assert!(q.removed.is_empty());
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
