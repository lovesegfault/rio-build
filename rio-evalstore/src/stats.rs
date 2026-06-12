//! Per-op count/bytes statistics, dumped to stderr on store drop when
//! `RIO_EVALSTORE_STATS=1`.
//!
//! ADR-024 records op size-class assumptions (eval reads tiny/per-call;
//! addToStore / narFromPath streamed) — these counters check them against
//! real evals instead of reasoning about them. Bytes are bucketed into a
//! coarse log-scale histogram per op.

use std::collections::BTreeMap;
use std::sync::Mutex;

/// Upper bounds (exclusive) of the per-call byte-size buckets.
const BUCKET_BOUNDS: [u64; 7] = [
    1 << 10,   // 1 KiB
    8 << 10,   // 8 KiB
    64 << 10,  // 64 KiB
    1 << 20,   // 1 MiB
    16 << 20,  // 16 MiB
    256 << 20, // 256 MiB
    u64::MAX,
];

const BUCKET_LABELS: [&str; 7] = ["<1K", "<8K", "<64K", "<1M", "<16M", "<256M", ">=256M"];

#[derive(Default, Clone)]
struct OpStat {
    calls: u64,
    bytes: u64,
    buckets: [u64; 7],
}

#[derive(Default)]
pub struct Stats {
    ops: Mutex<BTreeMap<&'static str, OpStat>>,
}

impl Stats {
    /// Record one call of `op` that moved `bytes` payload bytes (0 for
    /// metadata-only ops).
    pub fn record(&self, op: &'static str, bytes: u64) {
        let mut ops = self.ops.lock().expect("stats mutex poisoned");
        let stat = ops.entry(op).or_default();
        stat.calls += 1;
        stat.bytes += bytes;
        let idx = BUCKET_BOUNDS
            .iter()
            .position(|&b| bytes < b)
            .unwrap_or(BUCKET_BOUNDS.len() - 1);
        stat.buckets[idx] += 1;
    }

    /// Call count recorded for `op` (0 when never recorded). Test
    /// surface for structural assertions — e.g. "the warm path did
    /// zero directory-blob decodes" — without parsing [`Stats::render`].
    pub fn count(&self, op: &str) -> u64 {
        let ops = self.ops.lock().expect("stats mutex poisoned");
        ops.get(op).map_or(0, |s| s.calls)
    }

    /// Render the histogram table (one line per op).
    pub fn render(&self) -> String {
        let ops = self.ops.lock().expect("stats mutex poisoned");
        let mut out =
            String::from("rio-evalstore op stats (calls/bytes + per-call size histogram):\n");
        for (op, stat) in ops.iter() {
            let buckets: Vec<String> = stat
                .buckets
                .iter()
                .zip(BUCKET_LABELS)
                .filter(|(n, _)| **n > 0)
                .map(|(n, l)| format!("{l}:{n}"))
                .collect();
            out.push_str(&format!(
                "  {op:<24} calls={:<8} bytes={:<12} [{}]\n",
                stat.calls,
                stat.bytes,
                buckets.join(" ")
            ));
        }
        out
    }

    pub fn enabled() -> bool {
        std::env::var("RIO_EVALSTORE_STATS").is_ok_and(|v| v == "1")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn record_and_render_buckets() {
        let s = Stats::default();
        s.record("read_file", 100);
        s.record("read_file", 5 << 10);
        s.record("nar_from_path", 2 << 20);
        let r = s.render();
        assert!(r.contains("read_file"), "got: {r}");
        assert!(r.contains("calls=2"), "got: {r}");
        assert!(r.contains("<1K:1"), "got: {r}");
        assert!(r.contains("<8K:1"), "got: {r}");
        assert!(r.contains("<16M:1"), "got: {r}");
    }
}
