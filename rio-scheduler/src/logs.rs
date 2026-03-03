//! Per-derivation build-log ring buffers.
//!
//! Lives **outside** the DAG actor so that a chatty build (10k lines/sec)
//! can't fill the actor's bounded mpsc(10_000) channel with log traffic and
//! trip the 80%/60% backpressure hysteresis. The BuildExecution recv task
//! writes directly here; the actor only touches this indirectly (via the
//! `ForwardLogBatch` command for gateway-forward, and via the completion
//! flush trigger added in a later commit).
//!
//! ## Ordering guarantees
//!
//! DashMap is a sharded lock — writes to **different** drv_path keys are
//! truly concurrent. Writes to the **same** drv_path are serialized by the
//! shard's RwLock. In practice, a derivation builds on exactly one worker
//! at a time, and that worker's single BuildExecution recv task is the only
//! writer for that drv_path → no intra-key contention.
//!
//! Line ordering within a buffer is the `first_line_number` from the worker
//! (LogBatcher increments it monotonically per batch). We don't sort — we
//! assume batches arrive in order (same TCP stream, same task, no reorder).
//! If they don't, `read_since` will expose the gap to the caller, which is
//! correct (the caller sees what we have).

use std::collections::VecDeque;

use dashmap::DashMap;
use rio_proto::types::BuildLogBatch;

/// Max lines retained per derivation. Beyond this, oldest lines are evicted.
///
/// Sizing: 100k lines × ~100 bytes/line (typical build output) ≈ 10 MiB per
/// active derivation. With ~50 concurrent active derivations (realistic upper
/// bound for a single scheduler before backpressure kicks in on the actor
/// channel), that's ~500 MiB peak — acceptable for a scheduler process that
/// typically has GBs of headroom.
///
/// This cap exists for the pathological case: a build that spews millions of
/// lines before the size-limit check on the worker kills it. We don't want
/// one runaway build to OOM the scheduler while the worker-side limit catches up.
pub(crate) const RING_CAPACITY: usize = 100_000;

/// (absolute line number, line bytes). Line number is the worker-assigned
/// `first_line_number + offset_within_batch` — absolute across the whole
/// build, not batch-local.
type Line = (u64, Vec<u8>);

/// Per-derivation log ring buffers, keyed by `drv_path`.
///
/// `drv_path` (not `drv_hash`) because that's what `BuildLogBatch` carries.
/// A derivation is built exactly once even if N builds want it (DAG merging),
/// so one ring buffer per drv_path is correct — the S3 flush (C8) writes one
/// blob and N `build_logs` PG rows (one per interested build, same s3_key).
pub struct LogBuffers {
    buffers: DashMap<String, VecDeque<Line>>,
}

impl LogBuffers {
    pub fn new() -> Self {
        Self {
            buffers: DashMap::new(),
        }
    }

    /// Push a batch. Evicts oldest lines if the buffer exceeds [`RING_CAPACITY`].
    ///
    /// Returns `true` if this is the first batch for this `drv_path` (i.e., a
    /// new buffer was created). Callers can use this to know when a derivation
    /// first started producing output — potentially useful for the periodic
    /// flush to skip just-started derivations.
    pub fn push(&self, batch: &BuildLogBatch) -> bool {
        // `entry()` locks the shard's write lock for the duration of the
        // closure. For the same-key case (one worker per drv_path), this is
        // uncontended. For cross-key, DashMap's sharding means we rarely
        // block other drv_paths.
        let entry = self.buffers.entry(batch.derivation_path.clone());
        let is_new = matches!(&entry, dashmap::Entry::Vacant(_));
        let mut buf = entry.or_default();

        let base = batch.first_line_number;
        for (i, line) in batch.lines.iter().enumerate() {
            buf.push_back((base + i as u64, line.clone()));
        }

        // Evict oldest lines if over capacity. `pop_front` is O(1) on VecDeque.
        // We evict AFTER push (not before) so a single batch larger than
        // RING_CAPACITY doesn't leave the buffer empty — instead it keeps
        // the tail of that batch.
        while buf.len() > RING_CAPACITY {
            buf.pop_front();
        }

        is_new
    }

    /// Drain all lines for a derivation, removing the buffer entry.
    ///
    /// Called on completion flush (C8). Returns `None` if the buffer doesn't
    /// exist (never logged anything, or already drained).
    ///
    /// Returns `(line_count, total_bytes, lines_in_order)`. `line_count` may
    /// be less than the total emitted by the worker if ring eviction kicked
    /// in — in that case the earliest lines are gone, which the S3 blob
    /// will reflect (starts at a non-zero line number). That's the
    /// intentional tradeoff of a ring buffer.
    pub fn drain(&self, drv_path: &str) -> Option<(u64, u64, Vec<Vec<u8>>)> {
        let (_key, deque) = self.buffers.remove(drv_path)?;
        let line_count = deque.len() as u64;
        let mut total_bytes = 0u64;
        let lines: Vec<Vec<u8>> = deque
            .into_iter()
            .map(|(_n, bytes)| {
                total_bytes += bytes.len() as u64;
                bytes
            })
            .collect();
        Some((line_count, total_bytes, lines))
    }

    /// Read lines with line number ≥ `since`, non-consuming.
    ///
    /// For `AdminService.GetBuildLogs` (C9) — lets a late-joining dashboard
    /// client catch up from the ring buffer without blocking on S3.
    ///
    /// Returns `(line_number, line_bytes)` pairs. Empty vec if the buffer
    /// doesn't exist or has no lines ≥ `since` (after eviction, early
    /// line numbers may be gone — that's expected).
    pub fn read_since(&self, drv_path: &str, since: u64) -> Vec<Line> {
        let Some(buf) = self.buffers.get(drv_path) else {
            return Vec::new();
        };
        // Could binary-search for `since` since line numbers are monotone,
        // but `read_since` is called by dashboard polls (infrequent) on a
        // buffer that's already in-memory. Linear scan is fine until
        // profiling says otherwise; premature optimization would just
        // obscure the invariant that makes bisection valid.
        buf.iter().filter(|(n, _)| *n >= since).cloned().collect()
    }

    /// Discard a buffer without returning its contents.
    ///
    /// Called on worker disconnect mid-build when the derivation gets
    /// reassigned — the new worker's logs replace the old ones from scratch
    /// (the old partial log is meaningless without the build that produced it).
    pub fn discard(&self, drv_path: &str) {
        self.buffers.remove(drv_path);
    }

    /// Number of active buffers. For tests + metrics.
    #[cfg(test)]
    pub fn active_count(&self) -> usize {
        self.buffers.len()
    }
}

impl Default for LogBuffers {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mk_batch(drv_path: &str, first_line: u64, lines: &[&[u8]]) -> BuildLogBatch {
        BuildLogBatch {
            derivation_path: drv_path.to_string(),
            lines: lines.iter().map(|l| l.to_vec()).collect(),
            first_line_number: first_line,
            worker_id: "test-worker".into(),
        }
    }

    #[test]
    fn push_then_read_since_returns_all() {
        let bufs = LogBuffers::new();
        let is_new = bufs.push(&mk_batch("drv-a", 0, &[b"line0", b"line1", b"line2"]));
        assert!(is_new, "first push should report new buffer");

        let lines = bufs.read_since("drv-a", 0);
        assert_eq!(lines.len(), 3);
        assert_eq!(lines[0], (0, b"line0".to_vec()));
        assert_eq!(lines[1], (1, b"line1".to_vec()));
        assert_eq!(lines[2], (2, b"line2".to_vec()));
    }

    #[test]
    fn push_twice_second_is_not_new() {
        let bufs = LogBuffers::new();
        assert!(bufs.push(&mk_batch("drv-a", 0, &[b"l0"])));
        assert!(!bufs.push(&mk_batch("drv-a", 1, &[b"l1"])), "second push");
        assert_eq!(bufs.read_since("drv-a", 0).len(), 2);
    }

    #[test]
    fn read_since_filters_by_line_number() {
        let bufs = LogBuffers::new();
        bufs.push(&mk_batch("drv-a", 0, &[b"l0", b"l1", b"l2", b"l3", b"l4"]));
        let lines = bufs.read_since("drv-a", 3);
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[0].0, 3);
        assert_eq!(lines[1].0, 4);
    }

    #[test]
    fn read_since_nonexistent_buffer_empty() {
        let bufs = LogBuffers::new();
        assert!(bufs.read_since("not-there", 0).is_empty());
    }

    #[test]
    fn ring_eviction_drops_oldest() {
        let bufs = LogBuffers::new();
        // Fill to capacity.
        let lines: Vec<Vec<u8>> = (0..RING_CAPACITY).map(|i| format!("l{i}").into()).collect();
        let line_refs: Vec<&[u8]> = lines.iter().map(|v| v.as_slice()).collect();
        bufs.push(&mk_batch("drv-a", 0, &line_refs));
        assert_eq!(bufs.read_since("drv-a", 0).len(), RING_CAPACITY);

        // Push 100 more → oldest 100 evicted.
        let extra: Vec<Vec<u8>> = (0..100).map(|i| format!("x{i}").into()).collect();
        let extra_refs: Vec<&[u8]> = extra.iter().map(|v| v.as_slice()).collect();
        bufs.push(&mk_batch("drv-a", RING_CAPACITY as u64, &extra_refs));

        let all = bufs.read_since("drv-a", 0);
        assert_eq!(all.len(), RING_CAPACITY, "still at capacity after eviction");
        // The FIRST line number present should be 100 (lines 0-99 evicted).
        assert_eq!(all[0].0, 100, "oldest 100 should be evicted");
        // The LAST line should be the last extra line.
        assert_eq!(all.last().unwrap().0, RING_CAPACITY as u64 + 99);
    }

    #[test]
    fn single_batch_larger_than_capacity_keeps_tail() {
        // Edge case: one giant batch > RING_CAPACITY. We want the TAIL kept
        // (most recent lines), not an empty buffer.
        let bufs = LogBuffers::new();
        let big = RING_CAPACITY + 50;
        let lines: Vec<Vec<u8>> = (0..big).map(|i| vec![i as u8]).collect();
        let line_refs: Vec<&[u8]> = lines.iter().map(|v| v.as_slice()).collect();
        bufs.push(&mk_batch("drv-a", 0, &line_refs));

        let all = bufs.read_since("drv-a", 0);
        assert_eq!(all.len(), RING_CAPACITY);
        assert_eq!(all[0].0, 50, "first 50 evicted, kept tail");
        assert_eq!(all.last().unwrap().0, big as u64 - 1);
    }

    #[test]
    fn drain_removes_entry_and_returns_all() {
        let bufs = LogBuffers::new();
        bufs.push(&mk_batch("drv-a", 0, &[b"hello", b"world"]));
        assert_eq!(bufs.active_count(), 1);

        let (count, bytes, lines) = bufs.drain("drv-a").expect("buffer exists");
        assert_eq!(count, 2);
        assert_eq!(bytes, 5 + 5);
        assert_eq!(lines, vec![b"hello".to_vec(), b"world".to_vec()]);
        assert_eq!(bufs.active_count(), 0, "drain removed the entry");
        assert!(bufs.drain("drv-a").is_none(), "second drain returns None");
    }

    #[test]
    fn drain_nonexistent_returns_none() {
        let bufs = LogBuffers::new();
        assert!(bufs.drain("not-there").is_none());
    }

    #[test]
    fn discard_removes_without_returning() {
        let bufs = LogBuffers::new();
        bufs.push(&mk_batch("drv-a", 0, &[b"line"]));
        bufs.discard("drv-a");
        assert_eq!(bufs.active_count(), 0);
        assert!(bufs.read_since("drv-a", 0).is_empty());
    }

    #[test]
    fn separate_drv_paths_are_independent() {
        let bufs = LogBuffers::new();
        bufs.push(&mk_batch("drv-a", 0, &[b"a0"]));
        bufs.push(&mk_batch("drv-b", 0, &[b"b0", b"b1"]));

        assert_eq!(bufs.read_since("drv-a", 0).len(), 1);
        assert_eq!(bufs.read_since("drv-b", 0).len(), 2);

        bufs.drain("drv-a");
        assert_eq!(bufs.read_since("drv-b", 0).len(), 2, "b untouched");
    }

    /// DashMap's sharded locking should handle concurrent push from
    /// multiple tasks without panicking or losing lines. Different
    /// keys → truly concurrent; same key → serialized by shard lock.
    #[tokio::test]
    async fn concurrent_push_different_keys() {
        let bufs = std::sync::Arc::new(LogBuffers::new());
        let mut handles = Vec::new();
        for i in 0..16 {
            let bufs = bufs.clone();
            handles.push(tokio::spawn(async move {
                let drv = format!("drv-{i}");
                let five_lines: [&[u8]; 5] = [b"l", b"l", b"l", b"l", b"l"];
                for batch_n in 0..10 {
                    bufs.push(&mk_batch(&drv, batch_n * 5, &five_lines));
                }
            }));
        }
        for h in handles {
            h.await.unwrap();
        }
        assert_eq!(bufs.active_count(), 16);
        for i in 0..16 {
            assert_eq!(
                bufs.read_since(&format!("drv-{i}"), 0).len(),
                50,
                "drv-{i} should have all 50 lines"
            );
        }
    }
}
