//! Build log streaming via gRPC.
//!
//! Buffers STDERR_NEXT messages from `nix-daemon` and batches them into
//! `BuildLogBatch` messages (64 lines or 100ms, whichever comes first).
//! Batches are sent on the scheduler `BuildExecution` stream.
//!
//! Also enforces per-build log limits. `total_bytes` is a hard cap:
//! exceeding it returns [`AddLineResult::LimitExceeded`] and the caller
//! aborts the build with `BuildStatus::LogLimitExceeded`. `rate_lines_per_sec`
//! is a suppression threshold: excess lines in a 1s window are DROPPED
//! (not failed), and a single `[rio: N lines suppressed …]` marker is
//! injected at the next window reset. The size limit bounds infrastructure
//! cost; the rate limit bounds per-tick scheduler load without killing
//! legitimate bursty builds (kernel `make oldconfig` emits ~18k prompts in
//! one burst).

use std::time::{Duration, Instant};

use rio_proto::types::BuildLogBatch;

/// Maximum lines per batch.
const MAX_BATCH_LINES: usize = 64;

/// Maximum time to wait before flushing a partial batch.
pub(crate) const BATCH_TIMEOUT: Duration = Duration::from_millis(100);

/// Per-build log limits, enforced by [`LogBatcher`].
///
/// Both limits are **soft-off at 0** (unlimited).
#[derive(Debug, Clone, Copy)]
pub struct LogLimits {
    /// Max log lines per second before suppression kicks in. 0 = unlimited.
    ///
    /// Enforced via a 1-second tumbling window. Lines beyond the
    /// threshold within a window are dropped; a marker line is
    /// injected at the next window reset showing the drop count.
    /// `total_bytes` is the hard cap; this only bounds per-second
    /// scheduler-stream load.
    pub rate_lines_per_sec: u64,
    /// Max total log bytes across the whole build. 0 = unlimited.
    /// Exceeding this aborts the build (`BuildStatus::LogLimitExceeded`).
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
    /// Line accepted (buffered, or dropped by rate suppression — either
    /// way the caller continues). Batch not yet full.
    Buffered,
    /// Line completed a batch. Caller must send it.
    BatchReady(BuildLogBatch),
    /// `total_bytes` limit tripped. Caller must abort the build with
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
    executor_id: String,
    /// Accumulated log lines (raw bytes, may be non-UTF-8).
    lines: Vec<Vec<u8>>,
    /// Line number counter.
    next_line_number: u64,
    /// When the current batch started accumulating.
    batch_start: Instant,

    // --- limits ---
    limits: LogLimits,
    /// Lines accepted in the current rate window.
    lines_this_window: u64,
    /// Lines dropped (rate-suppressed) in the current rate window.
    lines_dropped_this_window: u64,
    /// Start of the current rate window.
    window_start: Instant,
    /// Total bytes across all lines ever added (including flushed batches).
    total_bytes: u64,
}

impl LogBatcher {
    /// Derivation path this batcher is collecting logs for. Exposed so
    /// the stderr loop can tag `BuildPhase` messages without threading
    /// `drv_path` separately through `read_build_stderr_loop`.
    pub fn drv_path(&self) -> &str {
        &self.drv_path
    }

    /// Create a new log batcher for the given derivation.
    pub fn new(drv_path: String, executor_id: String, limits: LogLimits) -> Self {
        let now = Instant::now();
        Self {
            drv_path,
            executor_id,
            lines: Vec::with_capacity(MAX_BATCH_LINES),
            next_line_number: 0,
            batch_start: now,
            limits,
            lines_this_window: 0,
            lines_dropped_this_window: 0,
            window_start: now,
            total_bytes: 0,
        }
    }

    /// Add a log line. Returns the batch if it's full, or a limit-exceeded
    /// signal if a limit tripped.
    ///
    /// Limit checks happen BEFORE buffering — a line that would exceed the
    /// size limit is rejected, not half-accepted.
    // r[impl builder.log-limit+2]
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

        // --- Rate limit (suppression, not abort) ---
        // Tumbling window: if ≥ 1s has elapsed since window_start, reset.
        // Instant (monotonic) so NTP jumps don't spuriously trip/un-trip.
        if self.limits.rate_lines_per_sec > 0 {
            if self.window_start.elapsed() >= Duration::from_secs(1) {
                let dropped = std::mem::take(&mut self.lines_dropped_this_window);
                self.window_start = Instant::now();
                self.lines_this_window = 0;
                if dropped > 0 {
                    metrics::counter!("rio_builder_log_lines_suppressed_total").increment(dropped);
                    // Marker is pushed directly (not via add_line) so
                    // it can't recurse and always lands ahead of `line`.
                    // It counts toward this window's quota and total_bytes
                    // (may overshoot the size limit by ~one marker; the
                    // very next line correctly trips LimitExceeded).
                    let marker = format!(
                        "[rio: {dropped} lines suppressed by log_rate_limit ({} lines/s)]",
                        self.limits.rate_lines_per_sec
                    )
                    .into_bytes();
                    self.total_bytes += marker.len() as u64;
                    self.lines.push(marker);
                    self.lines_this_window += 1;
                }
            }
            if self.lines_this_window >= self.limits.rate_lines_per_sec {
                self.lines_dropped_this_window += 1;
                return AddLineResult::Buffered;
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
    ///
    /// Drains `lines_dropped_this_window` first: a build whose final burst
    /// exceeds the rate and then exits within the same 1s window never
    /// triggers `add_line()`'s window-reset, so the suppression marker +
    /// metric would otherwise be lost (silent truncation, undercounted
    /// `rio_builder_log_lines_suppressed_total`). All exit paths
    /// (`STDERR_LAST`, `LimitExceeded`, silence-timeout, `BatchReady`) go
    /// through here.
    pub fn flush(&mut self) -> BuildLogBatch {
        let dropped = std::mem::take(&mut self.lines_dropped_this_window);
        if dropped > 0 {
            metrics::counter!("rio_builder_log_lines_suppressed_total").increment(dropped);
            let marker = format!(
                "[rio: {dropped} lines suppressed by log_rate_limit ({} lines/s)]",
                self.limits.rate_lines_per_sec
            )
            .into_bytes();
            self.total_bytes += marker.len() as u64;
            self.lines.push(marker);
        }
        let first_line_number = self.next_line_number;
        self.next_line_number += self.lines.len() as u64;

        let lines = std::mem::take(&mut self.lines);

        BuildLogBatch {
            derivation_path: self.drv_path.clone(),
            lines,
            first_line_number,
            executor_id: self.executor_id.clone(),
        }
    }

    /// Whether there are any buffered lines or an unreported suppression
    /// count.
    ///
    /// Used by `executor/daemon/stderr_loop.rs` to decide whether to
    /// `flush()` before sending the final batch on stream close.
    pub fn has_pending(&self) -> bool {
        !self.lines.is_empty() || self.lines_dropped_this_window > 0
    }
}

// r[verify obs.log.batch-64-100ms]
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
                assert_eq!(batch.executor_id, "worker-1");
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

    // r[verify builder.log-limit+2]
    #[test]
    fn rate_limit_drops_excess_within_window() {
        let mut batcher = mk(LogLimits {
            rate_lines_per_sec: 5,
            total_bytes: 0,
        });
        // 10 lines in <1s: first 5 buffered, next 5 dropped. ALL return
        // Buffered (drop is not an error).
        for i in 0..10 {
            match batcher.add_line(vec![i as u8]) {
                AddLineResult::Buffered => {}
                other => panic!("line {i} should be Buffered, got {other:?}"),
            }
        }
        let batch = batcher.flush();
        assert_eq!(batch.lines.len(), 6, "5 buffered + 1 suppression marker");
        assert_eq!(batch.lines[4], vec![4u8], "last buffered is index 4");
        let marker = std::str::from_utf8(&batch.lines[5]).unwrap();
        assert!(
            marker.contains("5 lines suppressed"),
            "flush() emits marker for drops never followed by window-reset: {marker}"
        );
    }

    /// Regression: build ends within the suppression window → flush() must
    /// emit the marker (was lost: only add_line()'s window-reset drained it).
    #[test]
    fn rate_limit_flush_emits_marker_when_build_ends_in_window() {
        let mut batcher = mk(LogLimits {
            rate_lines_per_sec: 3,
            total_bytes: 0,
        });
        for _ in 0..7 {
            batcher.add_line(b"l".to_vec());
        }
        // No sleep, no further add_line — straight to final flush.
        assert!(batcher.has_pending(), "has_pending sees unreported drops");
        let batch = batcher.flush();
        assert_eq!(batch.lines.len(), 4, "3 accepted + 1 marker");
        let marker = std::str::from_utf8(&batch.lines[3]).unwrap();
        assert!(marker.contains("4 lines suppressed"));
        assert!(!batcher.has_pending(), "drained after flush");
    }

    #[test]
    fn rate_limit_marker_on_window_reset() {
        let mut batcher = mk(LogLimits {
            rate_lines_per_sec: 3,
            total_bytes: 0,
        });
        // Fill the window: 3 accepted, 4 dropped.
        for _ in 0..7 {
            assert!(matches!(
                batcher.add_line(b"l".to_vec()),
                AddLineResult::Buffered
            ));
        }
        // Real-time sleep > 1s → window resets. (Instant doesn't advance
        // under paused tokio-time; rate limiting against wall-clock is
        // the point.)
        std::thread::sleep(Duration::from_millis(1100));
        // Next line: marker injected first (counts toward this window's
        // quota), then the line. Window quota is 3 → marker + 2 lines fit.
        for i in 0..2 {
            match batcher.add_line(b"m".to_vec()) {
                AddLineResult::Buffered => {}
                other => panic!("post-reset line {i} should be accepted, got {other:?}"),
            }
        }
        let batch = batcher.flush();
        // 3 (window 1) + 1 marker + 2 (window 2) = 6.
        assert_eq!(batch.lines.len(), 6);
        let marker = std::str::from_utf8(&batch.lines[3]).unwrap();
        assert!(
            marker.contains("4 lines suppressed") && marker.contains("log_rate_limit"),
            "marker at index 3 should report 4 drops: {marker}"
        );
    }

    #[test]
    fn rate_limit_no_marker_when_nothing_dropped() {
        let mut batcher = mk(LogLimits {
            rate_lines_per_sec: 3,
            total_bytes: 0,
        });
        for _ in 0..3 {
            batcher.add_line(b"l".to_vec());
        }
        std::thread::sleep(Duration::from_millis(1100));
        for _ in 0..3 {
            batcher.add_line(b"m".to_vec());
        }
        let batch = batcher.flush();
        assert_eq!(batch.lines.len(), 6, "no marker injected — nothing dropped");
        assert!(batch.lines.iter().all(|l| l == b"l" || l == b"m"));
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
    fn rate_dropped_lines_dont_count_toward_size() {
        // A rate-suppressed line is never buffered or transmitted, so it
        // costs nothing — it must NOT count toward total_bytes.
        let mut batcher = mk(LogLimits {
            rate_lines_per_sec: 1,
            total_bytes: 10,
        });
        // First line (5 bytes) buffered.
        assert!(matches!(
            batcher.add_line(vec![b'x'; 5]),
            AddLineResult::Buffered
        ));
        // 100 dropped lines (rate=1, window full). None counted toward size.
        for _ in 0..100 {
            assert!(matches!(
                batcher.add_line(vec![b'y'; 5]),
                AddLineResult::Buffered
            ));
        }
        // total_bytes is still 5. A 5-byte line fits (5+5=10 ≤ limit) —
        // but the rate window is still full so it's dropped too.
        // Prove via size: a 6-byte line would trip size if total_bytes
        // had been polluted by drops; since it's only 5, the size check
        // passes (5+6=11 > 10 → trips, which is CORRECT for the 5 real
        // bytes, not 505 phantom bytes).
        match batcher.add_line(vec![b'z'; 6]) {
            AddLineResult::LimitExceeded { reason } => {
                assert!(
                    reason.contains("11"),
                    "prospective should be 5+6=11: {reason}"
                );
            }
            other => panic!("expected size trip at 11 bytes, got {other:?}"),
        }
    }
}
