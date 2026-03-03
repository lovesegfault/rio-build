//! Build log streaming via gRPC.
//!
//! Buffers STDERR_NEXT messages from `nix-daemon` and batches them into
//! `BuildLogBatch` messages (64 lines or 100ms, whichever comes first).
//! Batches are sent on the scheduler `BuildExecution` stream.

use std::time::{Duration, Instant};

use rio_proto::types::BuildLogBatch;

/// Maximum lines per batch.
const MAX_BATCH_LINES: usize = 64;

/// Maximum time to wait before flushing a partial batch.
pub(crate) const BATCH_TIMEOUT: Duration = Duration::from_millis(100);

/// Log batcher that collects log lines and emits `BuildLogBatch` messages.
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
}

impl LogBatcher {
    /// Create a new log batcher for the given derivation.
    pub fn new(drv_path: String, worker_id: String) -> Self {
        Self {
            drv_path,
            worker_id,
            lines: Vec::with_capacity(MAX_BATCH_LINES),
            next_line_number: 0,
            batch_start: Instant::now(),
        }
    }

    /// Add a log line. Returns a batch if the batch is full.
    pub fn add_line(&mut self, line: Vec<u8>) -> Option<BuildLogBatch> {
        if self.lines.is_empty() {
            self.batch_start = Instant::now();
        }

        self.lines.push(line);

        if self.lines.len() >= MAX_BATCH_LINES {
            Some(self.flush())
        } else {
            None
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_batcher_accumulates_lines() {
        let mut batcher = LogBatcher::new("drv-path".into(), "worker-1".into());

        // Add fewer than 64 lines -- should not emit a batch
        for i in 0..63 {
            let result = batcher.add_line(format!("line {i}").into_bytes());
            assert!(result.is_none());
        }

        assert!(batcher.has_pending());
    }

    #[test]
    fn test_batcher_emits_at_64_lines() -> anyhow::Result<()> {
        let mut batcher = LogBatcher::new("drv-path".into(), "worker-1".into());

        for i in 0..63 {
            assert!(batcher.add_line(format!("line {i}").into_bytes()).is_none());
        }

        let batch = batcher.add_line(b"line 63".to_vec());
        assert!(batch.is_some());

        let batch = batch.expect("checked is_some");
        assert_eq!(batch.lines.len(), 64);
        assert_eq!(batch.first_line_number, 0);
        assert_eq!(batch.derivation_path, "drv-path");
        assert_eq!(batch.worker_id, "worker-1");
        Ok(())
    }

    #[test]
    fn test_batcher_line_numbers_increment() {
        let mut batcher = LogBatcher::new("drv-path".into(), "worker-1".into());

        // Fill and flush first batch
        for i in 0..64 {
            batcher.add_line(format!("line {i}").into_bytes());
        }

        // Second batch should start at line 64
        batcher.add_line(b"next".to_vec());
        let batch = batcher.flush();
        assert_eq!(batch.first_line_number, 64);
        assert_eq!(batch.lines.len(), 1);
    }

    #[test]
    fn test_batcher_flush_partial() {
        let mut batcher = LogBatcher::new("drv-path".into(), "worker-1".into());

        batcher.add_line(b"line 0".to_vec());
        batcher.add_line(b"line 1".to_vec());

        let batch = batcher.flush();
        assert_eq!(batch.lines.len(), 2);
        assert_eq!(batch.first_line_number, 0);

        assert!(!batcher.has_pending());
    }

    #[test]
    fn test_batcher_maybe_flush_no_timeout() {
        let mut batcher = LogBatcher::new("drv-path".into(), "worker-1".into());

        batcher.add_line(b"line".to_vec());
        // Should not flush immediately (timeout not elapsed)
        let result = batcher.maybe_flush();
        assert!(result.is_none());
    }

    #[test]
    fn test_batcher_maybe_flush_empty() {
        let mut batcher = LogBatcher::new("drv-path".into(), "worker-1".into());

        // Should not flush when empty
        let result = batcher.maybe_flush();
        assert!(result.is_none());
    }

    /// Verify the 100ms timeout causes a flush of a partial batch.
    /// Uses real time (LogBatcher uses Instant, not tokio::time).
    #[test]
    fn test_batcher_100ms_timeout_flush() -> anyhow::Result<()> {
        let mut batcher = LogBatcher::new("drv-path".into(), "worker-1".into());

        // Add a line — should not flush (< 64 lines)
        let result = batcher.add_line(b"line 0".to_vec());
        assert!(result.is_none());

        // Before timeout: maybe_flush returns None
        assert!(batcher.maybe_flush().is_none());

        // After timeout: maybe_flush returns the batch
        std::thread::sleep(std::time::Duration::from_millis(110));
        let batch = batcher.maybe_flush();
        assert!(batch.is_some(), "should flush after 100ms timeout");
        let batch = batch.expect("checked is_some");
        assert_eq!(batch.lines.len(), 1);
        assert_eq!(batch.lines[0], b"line 0");
        assert!(
            !batcher.has_pending(),
            "batcher should be empty after flush"
        );
        Ok(())
    }
}
