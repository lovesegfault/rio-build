//! FIFO ready queue for derivations awaiting worker assignment.
//!
//! Phase 2a uses a simple FIFO (VecDeque). IFD derivations are pushed to the
//! front of the queue for priority treatment.

use std::collections::VecDeque;

/// A FIFO queue of derivation hashes that are ready to be assigned to workers.
#[derive(Debug)]
pub struct ReadyQueue {
    /// The queue itself. Front is highest priority.
    queue: VecDeque<String>,
}

impl ReadyQueue {
    /// Create an empty ready queue.
    pub fn new() -> Self {
        Self {
            queue: VecDeque::new(),
        }
    }

    /// Push a derivation hash to the back of the queue (normal priority).
    pub fn push_back(&mut self, drv_hash: String) {
        // Avoid duplicates: don't push if already in queue
        if !self.queue.contains(&drv_hash) {
            self.queue.push_back(drv_hash);
        }
    }

    /// Push a derivation hash to the front of the queue (IFD priority).
    pub fn push_front(&mut self, drv_hash: String) {
        // Avoid duplicates: remove existing entry first
        if let Some(pos) = self.queue.iter().position(|h| h == &drv_hash) {
            self.queue.remove(pos);
        }
        self.queue.push_front(drv_hash);
    }

    /// Pop the next ready derivation hash from the front of the queue.
    pub fn pop_front(&mut self) -> Option<String> {
        self.queue.pop_front()
    }

    /// Peek at the front of the queue without removing.
    pub fn peek(&self) -> Option<&str> {
        self.queue.front().map(|s| s.as_str())
    }

    /// Remove a specific derivation hash from the queue (e.g., on cancellation).
    pub fn remove(&mut self, drv_hash: &str) -> bool {
        if let Some(pos) = self.queue.iter().position(|h| h == drv_hash) {
            self.queue.remove(pos);
            true
        } else {
            false
        }
    }

    /// Current number of items in the queue.
    pub fn len(&self) -> usize {
        self.queue.len()
    }

    /// Whether the queue is empty.
    pub fn is_empty(&self) -> bool {
        self.queue.is_empty()
    }

    /// Drain all items from the queue.
    pub fn drain(&mut self) -> impl Iterator<Item = String> + '_ {
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
        q.push_back("a".to_string());
        q.push_back("b".to_string());
        q.push_back("c".to_string());

        assert_eq!(q.pop_front(), Some("a".to_string()));
        assert_eq!(q.pop_front(), Some("b".to_string()));
        assert_eq!(q.pop_front(), Some("c".to_string()));
        assert_eq!(q.pop_front(), None);
    }

    #[test]
    fn test_ifd_priority() {
        let mut q = ReadyQueue::new();
        q.push_back("normal1".to_string());
        q.push_back("normal2".to_string());
        q.push_front("ifd".to_string());

        assert_eq!(q.pop_front(), Some("ifd".to_string()));
        assert_eq!(q.pop_front(), Some("normal1".to_string()));
        assert_eq!(q.pop_front(), Some("normal2".to_string()));
    }

    #[test]
    fn test_no_duplicates() {
        let mut q = ReadyQueue::new();
        q.push_back("a".to_string());
        q.push_back("a".to_string()); // duplicate
        assert_eq!(q.len(), 1);

        q.push_back("b".to_string());
        q.push_front("b".to_string()); // moves b to front
        assert_eq!(q.len(), 2);
        assert_eq!(q.pop_front(), Some("b".to_string()));
        assert_eq!(q.pop_front(), Some("a".to_string()));
    }

    #[test]
    fn test_remove() {
        let mut q = ReadyQueue::new();
        q.push_back("a".to_string());
        q.push_back("b".to_string());
        q.push_back("c".to_string());

        assert!(q.remove("b"));
        assert!(!q.remove("b")); // already removed
        assert_eq!(q.len(), 2);
        assert_eq!(q.pop_front(), Some("a".to_string()));
        assert_eq!(q.pop_front(), Some("c".to_string()));
    }
}
