//! Per-derivation build-log ring buffers.
//!
//! Lives **outside** the DAG actor so that a chatty build (10k lines/sec)
//! can't fill the actor's bounded mpsc(10_000) channel with log traffic and
//! trip the 80%/60% backpressure hysteresis. The BuildExecution recv task
// r[impl obs.log.batch-64-100ms]
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

use dashmap::{DashMap, DashSet};
use rio_nix::store_path::StorePath;
use rio_proto::types::BuildLogBatch;
use uuid::Uuid;

mod flush;
pub use flush::{FlushRequest, LogFlusher};

/// Extract the 32-char nixbase32 store-path hash from a derivation
/// identifier for use as the PG `build_logs.drv_hash` column and the
/// `{drv_hash}` component of the S3 key.
///
/// This is the SINGLE source of truth shared by the flusher (write side,
/// [`log_s3_key`] + `insert_log_rows`) and `AdminService.GetBuildLogs`
/// (read side, PG lookup) so the derivation can never drift. Before this
/// helper existed, the write side keyed on the full `/nix/store/...` path
/// while the read side keyed on the basename — the PG lookup never matched
/// and S3 keys had embedded `//nix/store/` (double-slash from
/// `format!("{prefix}/.../{full_path}")`).
///
/// Accepts any of:
/// - full store path `/nix/store/{hash}-{name}.drv` → `{hash}`
/// - basename `{hash}-{name}.drv` → `{hash}`
/// - bare hash `{hash}` → unchanged (dashboard passes this directly)
pub fn drv_log_hash(s: &str) -> String {
    // Full store path → parsed hash_part. Validates nixbase32 + length.
    if let Ok(sp) = StorePath::parse(s) {
        return sp.hash_part();
    }
    // Not a parseable store path (no prefix, short test hash, or invalid
    // name char). Best-effort: strip `/nix/store/` if present, then take
    // the part before the first `-`. No `-` → already hash-shaped.
    let base = rio_nix::store_path::basename(s).unwrap_or(s);
    base.split_once('-')
        .map(|(h, _)| h)
        .unwrap_or(base)
        .to_string()
}

/// Construct the canonical S3 key for a completed derivation's log blob:
/// `{prefix}/{build_id}/{drv_hash}.log.zst` per `observability.md`.
///
/// `drv_path` is the full `/nix/store/...` path (the ring-buffer key);
/// the hash is extracted via [`drv_log_hash`].
pub fn log_s3_key(prefix: &str, build_id: &Uuid, drv_path: &str) -> String {
    format!("{prefix}/{build_id}/{}.log.zst", drv_log_hash(drv_path))
}

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
/// so one ring buffer per drv_path is correct — the S3 flush writes one
/// blob and N `build_logs` PG rows (one per interested build, same s3_key).
pub struct LogBuffers {
    buffers: DashMap<String, VecDeque<Line>>,
    /// Tombstone set: derivations that have reached a terminal state.
    /// [`Self::push`] drops batches for sealed paths so a late
    /// `LogBatch` (still in flight on the BuildExecution stream after
    /// the worker sent CompletionReport) cannot recreate a buffer
    /// that the flusher already drained. Unsealed by the
    /// BuildExecution recv task on stream-close and by
    /// [`LogFlusher::flush_final`] post-drain — bounds `sealed` to
    /// drvs whose worker stream is still open.
    sealed: DashSet<String>,
}

impl LogBuffers {
    pub fn new() -> Self {
        Self {
            buffers: DashMap::new(),
            sealed: DashSet::new(),
        }
    }

    /// Push a batch. Evicts oldest lines if the buffer exceeds `RING_CAPACITY`.
    ///
    /// Drops the batch entirely if `drv_path` is [`Self::seal`]ed
    /// (terminal completion already fired). The late lines are lost —
    /// the build is done, the flusher has (or will) upload the final
    /// snapshot, and a few trailing batched lines are not worth an
    /// unbounded entry-count leak.
    pub fn push(&self, batch: &BuildLogBatch) {
        if self.sealed.contains(&batch.derivation_path) {
            return;
        }
        // `entry()` locks the shard's write lock for the duration of the
        // closure. For the same-key case (one worker per drv_path), this is
        // uncontended. For cross-key, DashMap's sharding means we rarely
        // block other drv_paths.
        let mut buf = self
            .buffers
            .entry(batch.derivation_path.clone())
            .or_default();

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
    }

    /// Drain all lines for a derivation, removing the buffer entry.
    ///
    /// Called on completion flush. Returns `None` if the buffer doesn't
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
    /// For `AdminService.GetBuildLogs` — lets a late-joining dashboard
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

    /// Discard a buffer without returning its contents. Also un-seals.
    ///
    /// Called by the actor's `assign_to_worker` (every fresh dispatch
    /// starts clean — clears a transient-failure predecessor's partial
    /// lines and any stale seal from a poison-clear) and by
    /// `handle_cleanup_terminal_build` for each reaped DAG node (bounds a
    /// dropped-FlushRequest leak to the ~30s cleanup delay). Idempotent;
    /// no-op on a missing entry.
    pub fn discard(&self, drv_path: &str) {
        self.buffers.remove(drv_path);
        self.sealed.remove(drv_path);
    }

    /// Mark `drv_path` terminal: subsequent [`Self::push`] calls drop.
    ///
    /// Called by the actor's completion handlers (`handle_success_completion`,
    /// `terminal_failure_epilogue`) BEFORE `trigger_log_flush`. The flusher's
    /// [`Self::drain`] still owns buffer removal — sealing only prevents
    /// post-drain recreation by a late batch. Any buffer present at seal
    /// time is left for the flusher; sealing then draining yields the same
    /// contents as draining alone.
    ///
    /// Idempotent. Retry / re-dispatch un-seals via [`Self::unseal`] (or
    /// [`Self::discard`], which also un-seals).
    pub fn seal(&self, drv_path: &str) {
        self.sealed.insert(drv_path.to_owned());
    }

    /// Reverse [`Self::seal`]: re-open `drv_path` for pushes. Called on
    /// re-dispatch after a terminal state (poison-clear, manual retry),
    /// and by the recv task / flusher on terminal cleanup to bound
    /// `sealed`. Idempotent; no-op if not sealed.
    pub fn unseal(&self, drv_path: &str) {
        self.sealed.remove(drv_path);
    }

    /// Whether `drv_path` is currently sealed (a completion landed and
    /// the flusher owns drain). The recv task's stream-exit cleanup
    /// branches on this: sealed → [`Self::unseal`] (leave the buffer
    /// for the flusher); not sealed → [`Self::discard`] (no completion
    /// → fake or aborted → reap so periodic-flush stops iterating it).
    pub fn is_sealed(&self, drv_path: &str) -> bool {
        self.sealed.contains(drv_path)
    }

    /// Number of active buffers. For metrics + flusher periodic-scan skip.
    pub fn active_count(&self) -> usize {
        self.buffers.len()
    }

    /// Number of sealed (tombstoned) drv_paths. Should hover near
    /// `active_count()` in steady state; unbounded growth = leak.
    pub fn sealed_count(&self) -> usize {
        self.sealed.len()
    }

    /// Snapshot all currently-buffered drv_paths (keys only, no lines).
    ///
    /// For the periodic flush. Snapshotting keys first (under DashMap's
    /// per-shard read locks) then draining each (under per-key write lock)
    /// avoids holding a shard lock across the slow S3 PUT. If a new
    /// drv_path starts buffering between the snapshot and the drain, it
    /// gets picked up on the NEXT periodic tick — no correctness issue.
    pub(crate) fn active_keys(&self) -> Vec<String> {
        self.buffers.iter().map(|e| e.key().clone()).collect()
    }

    /// Non-consuming clone of a buffer's contents (for periodic snapshot flush).
    ///
    /// Unlike `drain`, this does NOT remove the buffer — the derivation is
    /// still running, live serving via the ring buffer must continue, and
    /// the on-completion flush will drain+upload the final state. This
    /// means periodic snapshots upload an ever-growing prefix of the same
    /// log (wasteful in S3 PUTs, but bounded: at most one per 30s per active
    /// derivation, and the spec explicitly accepts that tradeoff at
    /// `observability.md:38-40`).
    pub(crate) fn snapshot(&self, drv_path: &str) -> Option<(u64, u64, Vec<Vec<u8>>)> {
        let buf = self.buffers.get(drv_path)?;
        let line_count = buf.len() as u64;
        let mut total_bytes = 0u64;
        let lines: Vec<Vec<u8>> = buf
            .iter()
            .map(|(_n, bytes)| {
                total_bytes += bytes.len() as u64;
                bytes.clone()
            })
            .collect();
        Some((line_count, total_bytes, lines))
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
            executor_id: "test-worker".into(),
        }
    }

    #[test]
    fn push_then_read_since_returns_all() {
        let bufs = LogBuffers::new();
        bufs.push(&mk_batch("drv-a", 0, &[b"line0", b"line1", b"line2"]));

        let lines = bufs.read_since("drv-a", 0);
        assert_eq!(lines.len(), 3);
        assert_eq!(lines[0], (0, b"line0".to_vec()));
        assert_eq!(lines[1], (1, b"line1".to_vec()));
        assert_eq!(lines[2], (2, b"line2".to_vec()));
    }

    #[test]
    fn push_twice_appends() {
        let bufs = LogBuffers::new();
        bufs.push(&mk_batch("drv-a", 0, &[b"l0"]));
        bufs.push(&mk_batch("drv-a", 1, &[b"l1"]));
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

    /// Regression: late LogBatch after completion must not recreate a
    /// drained entry. seal() tombstones the path so push() drops; the
    /// flusher's drain() still returns the pre-seal contents.
    #[test]
    fn seal_blocks_late_push_and_preserves_drain() {
        let bufs = LogBuffers::new();
        bufs.push(&mk_batch("drv-a", 0, &[b"line0", b"line1"]));

        // Actor seals on completion (BEFORE flusher drains).
        bufs.seal("drv-a");
        assert_eq!(bufs.sealed_count(), 1);

        // Late batch from the same BuildExecution stream — dropped.
        bufs.push(&mk_batch("drv-a", 2, &[b"late"]));

        // Flusher drains: gets the 2 pre-seal lines (seal did NOT
        // remove the buffer, only tombstoned it).
        let (count, _bytes, lines) = bufs.drain("drv-a").expect("buffer should exist");
        assert_eq!(count, 2);
        assert_eq!(lines, vec![b"line0".to_vec(), b"line1".to_vec()]);

        // Another late batch after drain — still sealed, still dropped.
        // This is the entry-count leak the seal closes: without it,
        // this push would recreate an orphan entry.
        bufs.push(&mk_batch("drv-a", 3, &[b"later"]));
        assert_eq!(
            bufs.active_count(),
            0,
            "sealed path must not recreate entry"
        );
        assert!(bufs.drain("drv-a").is_none());

        // Re-dispatch (poison-clear / retry) un-seals; new worker's
        // pushes land again.
        bufs.unseal("drv-a");
        assert_eq!(bufs.sealed_count(), 0);
        bufs.push(&mk_batch("drv-a", 0, &[b"retry"]));
        assert_eq!(bufs.active_count(), 1);
    }

    /// Regression: `sealed` must be cleared on terminal cleanup.
    /// Before the fix, `seal()` had no production remover — every
    /// completion leaked one String into `sealed` forever.
    #[test]
    fn seal_then_drain_then_unseal_clears_tombstone() {
        let bufs = LogBuffers::new();
        bufs.push(&mk_batch("drv-a", 0, &[b"l0"]));
        bufs.seal("drv-a");
        let _ = bufs.drain("drv-a");
        // drain() does NOT touch `sealed` — that's the leak shape.
        assert_eq!(bufs.sealed_count(), 1, "drain leaves seal in place");
        // Recv-task / flusher unseal is the bound.
        bufs.unseal("drv-a");
        assert_eq!(bufs.sealed_count(), 0);
        // Unseal re-opened: a fresh push lands.
        bufs.push(&mk_batch("drv-a", 0, &[b"fresh"]));
        assert_eq!(bufs.active_count(), 1);
    }

    /// Regression for merged_bug_128 secondary: transient-failure
    /// reassignment must not concatenate the old worker's partial lines
    /// with the new worker's. `assign_to_worker` now calls `discard()`
    /// before the new worker's first push.
    #[test]
    fn transient_retry_discard_clears_stale_partial() {
        let bufs = LogBuffers::new();
        // Worker W1 pushes 5 lines, then disconnects (transient failure).
        bufs.push(&mk_batch(
            "drv-a",
            0,
            &[b"w1-0", b"w1-1", b"w1-2", b"w1-3", b"w1-4"],
        ));
        // Re-dispatch: actor's assign_to_worker discards.
        bufs.discard("drv-a");
        // Worker W2 pushes from line 0 (fresh attempt).
        bufs.push(&mk_batch("drv-a", 0, &[b"w2-0", b"w2-1", b"w2-2"]));
        // Final flush drains: must be exactly W2's 3 lines, not 8.
        let (count, _bytes, lines) = bufs.drain("drv-a").expect("buffer exists");
        assert_eq!(count, 3, "stale W1 partial must be gone");
        assert_eq!(
            lines,
            vec![b"w2-0".to_vec(), b"w2-1".to_vec(), b"w2-2".to_vec()]
        );
    }

    /// Regression for bug_241: a dropped FlushRequest (channel-full burst)
    /// leaves the buffer in place; before the fix nothing ever removed it
    /// (perpetual 30s S3 PUTs + ~10MiB held). `CleanupTerminalBuild` now
    /// discards reaped nodes' buffers.
    #[test]
    fn dropped_flush_request_buffer_reaped_by_cleanup_discard() {
        let bufs = LogBuffers::new();
        bufs.push(&mk_batch("drv-a", 0, &[b"l0", b"l1"]));
        // Actor seals on completion, then try_send fails (channel full) —
        // flush_final never runs. Buffer is still present + sealed.
        bufs.seal("drv-a");
        assert_eq!(bufs.active_count(), 1, "dropped request leaves buffer");
        assert_eq!(bufs.sealed_count(), 1);
        // ~30s later: CleanupTerminalBuild reaps the DAG node and discards.
        bufs.discard("drv-a");
        assert_eq!(bufs.active_count(), 0, "cleanup discard frees buffer");
        assert_eq!(bufs.sealed_count(), 0, "cleanup discard also unseals");
    }

    #[test]
    fn discard_also_unseals() {
        let bufs = LogBuffers::new();
        bufs.seal("drv-a");
        bufs.discard("drv-a");
        bufs.push(&mk_batch("drv-a", 0, &[b"fresh"]));
        assert_eq!(bufs.active_count(), 1, "discard must clear seal");
    }
}
