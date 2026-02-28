//! FIFO ready queue for derivations awaiting worker assignment.
//!
//! Phase 2a uses a simple FIFO (VecDeque). IFD derivations are pushed to the
//! front of the queue for priority treatment.

use std::collections::{HashSet, VecDeque};

use crate::state::DrvHash;

/// A FIFO queue of derivation hashes that are ready to be assigned to workers.
///
/// Maintains a companion `HashSet` for O(1) membership checks: the
/// `dispatch_ready` defer loop re-queues all deferred items via
/// `push_front`, which would be O(n²) on a large queue with the naive
/// `VecDeque::contains` approach. The HashSet keeps `push_back` O(1)
/// average and short-circuits the "not present" case in `remove`.
#[derive(Debug)]
pub struct ReadyQueue {
    /// The queue itself. Front is highest priority.
    queue: VecDeque<DrvHash>,
    /// Membership index for O(1) dedup. Invariant: `members` contains
    /// exactly the elements of `queue`.
    members: HashSet<DrvHash>,
}

impl ReadyQueue {
    /// Create an empty ready queue.
    pub fn new() -> Self {
        Self {
            queue: VecDeque::new(),
            members: HashSet::new(),
        }
    }

    /// Push a derivation hash to the back of the queue (normal priority).
    /// O(1) average (HashSet membership check + insert).
    pub fn push_back(&mut self, drv_hash: DrvHash) {
        if self.members.insert(drv_hash.clone()) {
            self.queue.push_back(drv_hash);
        }
    }

    /// Push a derivation hash to the front of the queue (IFD priority).
    /// O(1) for new items; O(n) for move-to-front (rare: IFD re-prioritization
    /// of already-queued item).
    pub fn push_front(&mut self, drv_hash: DrvHash) {
        if self.members.insert(drv_hash.clone()) {
            // New item: just push to front.
            self.queue.push_front(drv_hash);
        } else {
            // Already in queue: move to front. O(n) scan unavoidable
            // without an index, but rare (IFD re-prioritization).
            if let Some(pos) = self.queue.iter().position(|h| h == &drv_hash) {
                self.queue.remove(pos);
                self.queue.push_front(drv_hash);
            }
        }
    }

    /// Pop the next ready derivation hash from the front of the queue.
    /// O(1) average.
    pub fn pop_front(&mut self) -> Option<DrvHash> {
        let h = self.queue.pop_front()?;
        self.members.remove(h.as_str());
        Some(h)
    }

    /// Peek at the front of the queue without removing.
    pub fn peek(&self) -> Option<&str> {
        self.queue.front().map(|s| s.as_str())
    }

    /// Remove a specific derivation hash from the queue (e.g., on cancellation).
    /// O(1) for the "not present" case (HashSet check); O(n) for the
    /// present case (VecDeque position scan).
    pub fn remove(&mut self, drv_hash: &str) -> bool {
        if self.members.remove(drv_hash) {
            if let Some(pos) = self.queue.iter().position(|h| h.as_str() == drv_hash) {
                self.queue.remove(pos);
            }
            true
        } else {
            false
        }
    }

    /// Current number of items in the queue.
    pub fn len(&self) -> usize {
        debug_assert_eq!(
            self.queue.len(),
            self.members.len(),
            "ReadyQueue invariant violated: queue/members size mismatch"
        );
        self.queue.len()
    }

    /// Whether the queue is empty.
    pub fn is_empty(&self) -> bool {
        self.queue.is_empty()
    }

    /// Drain all items from the queue.
    pub fn drain(&mut self) -> impl Iterator<Item = DrvHash> + '_ {
        self.members.clear();
        self.queue.drain(..)
    }

    /// Iterate over all items in the queue without removing them.
    pub fn iter(&self) -> impl Iterator<Item = &str> {
        self.queue.iter().map(|s| s.as_str())
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
    fn test_fifo_order() {
        let mut q = ReadyQueue::new();
        q.push_back("a".into());
        q.push_back("b".into());
        q.push_back("c".into());

        assert_eq!(q.pop_front(), Some("a".into()));
        assert_eq!(q.pop_front(), Some("b".into()));
        assert_eq!(q.pop_front(), Some("c".into()));
        assert_eq!(q.pop_front(), None);
    }

    #[test]
    fn test_ifd_priority() {
        let mut q = ReadyQueue::new();
        q.push_back("normal1".into());
        q.push_back("normal2".into());
        q.push_front("ifd".into());

        assert_eq!(q.pop_front(), Some("ifd".into()));
        assert_eq!(q.pop_front(), Some("normal1".into()));
        assert_eq!(q.pop_front(), Some("normal2".into()));
    }

    #[test]
    fn test_no_duplicates() {
        let mut q = ReadyQueue::new();
        q.push_back("a".into());
        q.push_back("a".into()); // duplicate
        assert_eq!(q.len(), 1);

        q.push_back("b".into());
        q.push_front("b".into()); // moves b to front
        assert_eq!(q.len(), 2);
        assert_eq!(q.pop_front(), Some("b".into()));
        assert_eq!(q.pop_front(), Some("a".into()));
    }

    #[test]
    fn test_remove() {
        let mut q = ReadyQueue::new();
        q.push_back("a".into());
        q.push_back("b".into());
        q.push_back("c".into());

        assert!(q.remove("b"));
        assert!(!q.remove("b")); // already removed
        assert_eq!(q.len(), 2);
        assert_eq!(q.pop_front(), Some("a".into()));
        assert_eq!(q.pop_front(), Some("c".into()));
    }

    /// Stress test: 10k pushes of the same key should never duplicate
    /// (HashSet dedup), and the membership invariant should hold.
    #[test]
    fn test_no_duplicates_with_10k_pushes() {
        let mut q = ReadyQueue::new();
        for i in 0..10_000 {
            if i % 2 == 0 {
                q.push_back("same".into());
            } else {
                q.push_front("same".into());
            }
        }
        assert_eq!(q.len(), 1);
        assert_eq!(q.pop_front(), Some("same".into()));
        assert!(q.is_empty());
    }

    /// Stress test: 10k distinct pushes + removes should not desync
    /// queue/members (invariant debug_assert in len()).
    #[test]
    fn test_queue_members_invariant_stress() {
        let mut q = ReadyQueue::new();
        for i in 0..10_000 {
            q.push_back(format!("item{i}").into());
        }
        assert_eq!(q.len(), 10_000);

        // Remove every other item.
        for i in (0..10_000).step_by(2) {
            assert!(q.remove(&format!("item{i}")));
        }
        assert_eq!(q.len(), 5_000);

        // Drain the rest; invariant checked in len() and at end.
        let drained: Vec<_> = q.drain().collect();
        assert_eq!(drained.len(), 5_000);
        assert!(q.is_empty());
    }
}
