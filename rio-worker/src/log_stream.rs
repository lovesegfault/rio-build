//! Build log streaming via gRPC.
//!
//! Buffers STDERR_NEXT messages from `nix-daemon` and batches them into
//! `BuildLogBatch` messages (64 lines or 100ms, whichever comes first).
//! Batches are sent on the scheduler `BuildExecution` stream.
//!
//! Also enforces per-build log limits (rate + total size). When a limit
//! trips, [`add_line`](LogBatcher::add_line) returns
//! [`AddLineResult::LimitExceeded`] and the caller (executor/daemon.rs)
//! aborts the build with `BuildStatus::LogLimitExceeded` — a terminal,
//! non-retryable failure (the same build on a different worker will just
//! spew the same logs).

use std::time::{Duration, Instant};

use rio_proto::types::BuildLogBatch;

/// Maximum lines per batch.
const MAX_BATCH_LINES: usize = 64;

/// Maximum time to wait before flushing a partial batch.
pub(crate) const BATCH_TIMEOUT: Duration = Duration::from_millis(100);

/// Per-build log limits, enforced by [`LogBatcher`].
///
/// Both limits are **soft-off at 0** (unlimited). The defaults
/// (`configuration.md:68-69`: 10k lines/s, 100 MiB total) are large enough
/// that only pathological builds — `set -x` in a tight loop, `yes` piped
/// somewhere — will hit them. Normal builds (even very verbose ones like
/// the linux kernel) stay well under.
#[derive(Debug, Clone, Copy)]
pub struct LogLimits {
    /// Max log lines per second. 0 = unlimited.
    ///
    /// Enforced via a simple 1-second tumbling window (no token bucket).
    /// Rationale: the only case that matters is sustained spew — a
    /// millisecond-scale burst of 11k lines that then quiets down is
    /// fine, but 10k+/s *sustained* for more than a second is always
    /// a bug. A tumbling window catches exactly that with zero state
    /// beyond one counter + one Instant.
    pub rate_lines_per_sec: u64,
    /// Max total log bytes across the whole build. 0 = unlimited.
    pub total_bytes: u64,
}

impl LogLimits {
    /// No limits. For tests where log limiting isn't the subject under test.
    pub const UNLIMITED: Self = Self {
        rate_lines_per_sec: 0,
        total_bytes: 0,
    };
}

/// Result of [`LogBatcher::add_line`].
#[derive(Debug)]
pub enum AddLineResult {
    /// Line buffered; batch not yet full.
    Buffered,
    /// Line completed a batch. Caller must send it.
    BatchReady(BuildLogBatch),
    /// A log limit tripped. Caller must abort the build with
    /// `BuildStatus::LogLimitExceeded` and the given reason in `error_msg`.
    ///
    /// The line that tripped the limit is **not** buffered (we're done
    /// accepting lines). Any already-buffered lines are still in the
    /// batcher — caller should `flush()` them before aborting so the
    /// client sees output right up to the limit.
    LimitExceeded { reason: String },
}

/// Log batcher that collects log lines, emits `BuildLogBatch` messages,
/// and enforces per-build log limits.
pub struct LogBatcher {
    /// Derivation path this batcher is collecting logs for.
    drv_path: String,
    /// Worker ID for the batch messages.
    worker_id: String,
    /// Accumulated log lines (raw bytes, may be non-UTF-8).
    lines: Vec<Vec<u8>>,
    /// Line number counter.
    next_line_number: u64,
    /// When the current batch started accumulating.
    batch_start: Instant,

    // --- limits ---
    limits: LogLimits,
    /// Lines added in the current rate window.
    lines_this_window: u64,
    /// Start of the current rate window.
    window_start: Instant,
    /// Total bytes across all lines ever added (including flushed batches).
    total_bytes: u64,
}

impl LogBatcher {
    /// Create a new log batcher for the given derivation.
    pub fn new(drv_path: String, worker_id: String, limits: LogLimits) -> Self {
        let now = Instant::now();
        Self {
            drv_path,
            worker_id,
            lines: Vec::with_capacity(MAX_BATCH_LINES),
            next_line_number: 0,
            batch_start: now,
            limits,
            lines_this_window: 0,
            window_start: now,
            total_bytes: 0,
        }
    }

    /// Add a log line. Returns the batch if it's full, or a limit-exceeded
    /// signal if a limit tripped.
    ///
    /// Limit checks happen BEFORE buffering — a line that would exceed the
    /// size limit is rejected, not half-accepted.
    pub fn add_line(&mut self, line: Vec<u8>) -> AddLineResult {
        // --- Size limit ---
        // Check the PROSPECTIVE total, not the current one. A 100 MiB limit
        // with 99.9 MiB accumulated and a 1 MiB line coming in should reject
        // that line, not accept it and trip on the NEXT one (which would put
        // us at 100.9 MiB — over the limit we're supposed to enforce).
        if self.limits.total_bytes > 0 {
            let prospective = self.total_bytes.saturating_add(line.len() as u64);
            if prospective > self.limits.total_bytes {
                return AddLineResult::LimitExceeded {
                    reason: format!(
                        "log_size_limit exceeded: {prospective} bytes > {} limit \
                         ({} lines so far)",
                        self.limits.total_bytes, self.next_line_number
                    ),
                };
            }
        }

        // --- Rate limit ---
        // Tumbling window: if > 1s has elapsed since window_start, reset.
        // We use Instant (monotonic, not wall-clock) so NTP jumps don't
        // spuriously trip or un-trip the limit.
        if self.limits.rate_lines_per_sec > 0 {
            if self.window_start.elapsed() >= Duration::from_secs(1) {
                self.window_start = Instant::now();
                self.lines_this_window = 0;
            }
            // Check BEFORE incrementing (same prospective-check logic as size).
            if self.lines_this_window >= self.limits.rate_lines_per_sec {
                return AddLineResult::LimitExceeded {
                    reason: format!(
                        "log_rate_limit exceeded: > {} lines/sec \
                         (window started {}ms ago, {} lines total so far)",
                        self.limits.rate_lines_per_sec,
                        self.window_start.elapsed().as_millis(),
                        self.next_line_number
                    ),
                };
            }
            self.lines_this_window += 1;
        }

        // --- Accept the line ---
        if self.lines.is_empty() {
            self.batch_start = Instant::now();
        }
        self.total_bytes += line.len() as u64;
        self.lines.push(line);

        if self.lines.len() >= MAX_BATCH_LINES {
            AddLineResult::BatchReady(self.flush())
        } else {
            AddLineResult::Buffered
        }
    }

    /// Check if the batch timeout has elapsed and flush if so.
    pub fn maybe_flush(&mut self) -> Option<BuildLogBatch> {
        if !self.lines.is_empty() && self.batch_start.elapsed() >= BATCH_TIMEOUT {
            Some(self.flush())
        } else {
            None
        }
    }

    /// Flush any remaining lines as a final batch.
    pub fn flush(&mut self) -> BuildLogBatch {
        let first_line_number = self.next_line_number;
        self.next_line_number += self.lines.len() as u64;

        let lines = std::mem::take(&mut self.lines);

        BuildLogBatch {
            derivation_path: self.drv_path.clone(),
            lines,
            first_line_number,
            worker_id: self.worker_id.clone(),
        }
    }

    /// Whether there are any buffered lines.
    pub fn has_pending(&self) -> bool {
        !self.lines.is_empty()
    }
}

// r[verify obs.log.batch-64-100ms]
// r[verify obs.metric.worker]
#[cfg(test)]
mod tests {
    use super::*;

    fn mk(limits: LogLimits) -> LogBatcher {
        LogBatcher::new("drv-path".into(), "worker-1".into(), limits)
    }

    // -----------------------------------------------------------------------
    // Batching (unchanged behavior, new return type)
    // -----------------------------------------------------------------------

    #[test]
    fn test_batcher_accumulates_lines() {
        let mut batcher = mk(LogLimits::UNLIMITED);
        for i in 0..63 {
            let result = batcher.add_line(format!("line {i}").into_bytes());
            assert!(matches!(result, AddLineResult::Buffered));
        }
        assert!(batcher.has_pending());
    }

    #[test]
    fn test_batcher_emits_at_64_lines() {
        let mut batcher = mk(LogLimits::UNLIMITED);
        for i in 0..63 {
            assert!(matches!(
                batcher.add_line(format!("line {i}").into_bytes()),
                AddLineResult::Buffered
            ));
        }
        match batcher.add_line(b"line 63".to_vec()) {
            AddLineResult::BatchReady(batch) => {
                assert_eq!(batch.lines.len(), 64);
                assert_eq!(batch.first_line_number, 0);
                assert_eq!(batch.derivation_path, "drv-path");
                assert_eq!(batch.worker_id, "worker-1");
            }
            other => panic!("expected BatchReady, got {other:?}"),
        }
    }

    #[test]
    fn test_batcher_line_numbers_increment() {
        let mut batcher = mk(LogLimits::UNLIMITED);
        for i in 0..64 {
            batcher.add_line(format!("line {i}").into_bytes());
        }
        batcher.add_line(b"next".to_vec());
        let batch = batcher.flush();
        assert_eq!(batch.first_line_number, 64);
        assert_eq!(batch.lines.len(), 1);
    }

    #[test]
    fn test_batcher_flush_partial() {
        let mut batcher = mk(LogLimits::UNLIMITED);
        batcher.add_line(b"line 0".to_vec());
        batcher.add_line(b"line 1".to_vec());
        let batch = batcher.flush();
        assert_eq!(batch.lines.len(), 2);
        assert_eq!(batch.first_line_number, 0);
        assert!(!batcher.has_pending());
    }

    #[test]
    fn test_batcher_maybe_flush_no_timeout() {
        let mut batcher = mk(LogLimits::UNLIMITED);
        batcher.add_line(b"line".to_vec());
        assert!(batcher.maybe_flush().is_none());
    }

    #[test]
    fn test_batcher_maybe_flush_empty() {
        let mut batcher = mk(LogLimits::UNLIMITED);
        assert!(batcher.maybe_flush().is_none());
    }

    /// Verify the 100ms timeout causes a flush of a partial batch.
    /// Uses real time (LogBatcher uses Instant, not tokio::time).
    #[test]
    fn test_batcher_100ms_timeout_flush() {
        let mut batcher = mk(LogLimits::UNLIMITED);
        assert!(matches!(
            batcher.add_line(b"line 0".to_vec()),
            AddLineResult::Buffered
        ));
        assert!(batcher.maybe_flush().is_none());
        std::thread::sleep(std::time::Duration::from_millis(110));
        let batch = batcher.maybe_flush().expect("should flush after 100ms");
        assert_eq!(batch.lines.len(), 1);
        assert_eq!(batch.lines[0], b"line 0");
        assert!(!batcher.has_pending());
    }

    // -----------------------------------------------------------------------
    // Rate limiting
    // -----------------------------------------------------------------------

    #[test]
    fn rate_limit_trips_at_threshold() {
        let mut batcher = mk(LogLimits {
            rate_lines_per_sec: 5,
            total_bytes: 0,
        });
        // First 5 lines accepted.
        for i in 0..5 {
            match batcher.add_line(vec![i as u8]) {
                AddLineResult::Buffered => {}
                other => panic!("line {i} should be accepted, got {other:?}"),
            }
        }
        // 6th line trips.
        match batcher.add_line(b"sixth".to_vec()) {
            AddLineResult::LimitExceeded { reason } => {
                assert!(
                    reason.contains("log_rate_limit"),
                    "reason should name the limit: {reason}"
                );
                assert!(reason.contains("5"), "reason should mention the threshold");
            }
            other => panic!("6th line should trip rate limit, got {other:?}"),
        }
        // The tripping line is NOT buffered. 5 lines should be pending.
        assert_eq!(batcher.flush().lines.len(), 5);
    }

    #[test]
    fn rate_limit_resets_after_window() {
        let mut batcher = mk(LogLimits {
            rate_lines_per_sec: 3,
            total_bytes: 0,
        });
        // Fill the window.
        for _ in 0..3 {
            assert!(matches!(
                batcher.add_line(b"l".to_vec()),
                AddLineResult::Buffered
            ));
        }
        // 4th would trip...
        // But sleep > 1s → window resets. (Real-time test — Instant doesn't
        // advance under paused tokio-time, and rate limiting against real
        // wall-clock is the point.)
        std::thread::sleep(Duration::from_millis(1100));
        // Now 3 more lines should be accepted.
        for i in 0..3 {
            match batcher.add_line(b"l".to_vec()) {
                AddLineResult::Buffered => {}
                other => panic!("post-reset line {i} should be accepted, got {other:?}"),
            }
        }
    }

    #[test]
    fn rate_limit_zero_means_unlimited() {
        let mut batcher = mk(LogLimits {
            rate_lines_per_sec: 0,
            total_bytes: 0,
        });
        // 1000 lines in rapid succession — no trip.
        for _ in 0..1000 {
            // Some of these will be BatchReady (every 64th), none should trip.
            if let AddLineResult::LimitExceeded { reason } = batcher.add_line(b"x".to_vec()) {
                panic!("rate=0 should be unlimited, got: {reason}")
            }
        }
    }

    // -----------------------------------------------------------------------
    // Size limiting
    // -----------------------------------------------------------------------

    #[test]
    fn size_limit_trips_on_prospective_total() {
        let mut batcher = mk(LogLimits {
            rate_lines_per_sec: 0,
            total_bytes: 100,
        });
        // 50 + 40 = 90 bytes, under limit.
        assert!(matches!(
            batcher.add_line(vec![b'a'; 50]),
            AddLineResult::Buffered
        ));
        assert!(matches!(
            batcher.add_line(vec![b'b'; 40]),
            AddLineResult::Buffered
        ));
        // 90 + 20 = 110 > 100 — trips BEFORE buffering (prospective check).
        match batcher.add_line(vec![b'c'; 20]) {
            AddLineResult::LimitExceeded { reason } => {
                assert!(reason.contains("log_size_limit"));
                assert!(reason.contains("110"), "should show prospective total");
                assert!(reason.contains("100"), "should show the limit");
            }
            other => panic!("should trip size limit, got {other:?}"),
        }
        // The 90-byte pre-trip content is still buffered.
        let batch = batcher.flush();
        assert_eq!(batch.lines.len(), 2);
        assert_eq!(batch.lines[0].len() + batch.lines[1].len(), 90);
    }

    #[test]
    fn size_limit_exactly_at_threshold_is_ok() {
        // Edge case: exactly hitting the limit should be accepted.
        // Only EXCEEDING it trips. (`>` not `>=` in the check.)
        let mut batcher = mk(LogLimits {
            rate_lines_per_sec: 0,
            total_bytes: 100,
        });
        assert!(matches!(
            batcher.add_line(vec![b'x'; 100]),
            AddLineResult::Buffered
        ));
        // Next byte trips.
        assert!(matches!(
            batcher.add_line(vec![b'y'; 1]),
            AddLineResult::LimitExceeded { .. }
        ));
    }

    #[test]
    fn size_limit_tracks_across_batches() {
        // total_bytes accumulates across flushed batches, not just the
        // current buffer. 64-line batch flush doesn't reset the counter.
        let mut batcher = mk(LogLimits {
            rate_lines_per_sec: 0,
            total_bytes: 70, // just over one full batch of 1-byte lines
        });
        for i in 0..64 {
            match batcher.add_line(vec![b'x'; 1]) {
                AddLineResult::Buffered => {}
                AddLineResult::BatchReady(_) => {
                    assert_eq!(i, 63, "batch should flush on 64th line")
                }
                AddLineResult::LimitExceeded { .. } => panic!("64 bytes < 70 limit"),
            }
        }
        // Now at 64 bytes. 6 more fit (= 70), 7th trips.
        for _ in 0..6 {
            assert!(matches!(
                batcher.add_line(vec![b'x'; 1]),
                AddLineResult::Buffered
            ));
        }
        assert!(matches!(
            batcher.add_line(vec![b'x'; 1]),
            AddLineResult::LimitExceeded { .. }
        ));
    }

    #[test]
    fn size_limit_zero_means_unlimited() {
        let mut batcher = mk(LogLimits {
            rate_lines_per_sec: 0,
            total_bytes: 0,
        });
        // 10 MiB across many lines — no trip.
        for _ in 0..100 {
            if let AddLineResult::LimitExceeded { reason } = batcher.add_line(vec![b'x'; 100_000]) {
                panic!("size=0 should be unlimited, got: {reason}")
            }
        }
    }

    #[test]
    fn both_limits_size_checked_first() {
        // If both would trip on the same line, we get a size reason (checked
        // first in the code). This isn't a contract, just documents current
        // behavior — either reason is valid, but consistency helps log grep.
        let mut batcher = mk(LogLimits {
            rate_lines_per_sec: 1,
            total_bytes: 1,
        });
        batcher.add_line(vec![b'x'; 1]); // uses rate quota AND size quota
        match batcher.add_line(vec![b'y'; 1]) {
            AddLineResult::LimitExceeded { reason } => {
                assert!(reason.contains("log_size_limit"), "size checked first");
            }
            other => panic!("expected limit exceeded, got {other:?}"),
        }
    }
}
