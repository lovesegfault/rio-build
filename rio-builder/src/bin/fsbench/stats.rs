//! Percentile math for latency samples.
//!
//! Nearest-rank percentiles with explicit sample-count floors: a p99
//! quoted from 50 samples is noise wearing a suit, so p99 needs
//! n ≥ 1000 and p999 needs n ≥ 10000 — below the floor the field is
//! omitted (and the omission is visible in the PERF line as an absent
//! key, not a zero).

pub const P99_FLOOR: usize = 1000;
pub const P999_FLOOR: usize = 10_000;

#[derive(Debug, Clone, serde::Serialize)]
pub struct Summary {
    pub n: usize,
    pub p50: u64,
    pub p90: u64,
    /// `None` below [`P99_FLOOR`].
    pub p99: Option<u64>,
    /// `None` below [`P999_FLOOR`].
    pub p999: Option<u64>,
    pub max: u64,
}

/// Sorts `samples` in place and summarizes. Empty input is a caller
/// bug (a phase that measured nothing) — panic beats quoting zeros.
pub fn summarize(samples: &mut [u64]) -> Summary {
    assert!(!samples.is_empty(), "summarize() on zero samples");
    samples.sort_unstable();
    Summary {
        n: samples.len(),
        p50: percentile(samples, 0.50),
        p90: percentile(samples, 0.90),
        p99: (samples.len() >= P99_FLOOR).then(|| percentile(samples, 0.99)),
        p999: (samples.len() >= P999_FLOOR).then(|| percentile(samples, 0.999)),
        max: *samples.last().expect("non-empty"),
    }
}

/// Nearest-rank on an ascending-sorted slice.
fn percentile(sorted: &[u64], q: f64) -> u64 {
    let rank = ((q * sorted.len() as f64).ceil() as usize).clamp(1, sorted.len());
    sorted[rank - 1]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nearest_rank_on_known_data() {
        // 1..=100: nearest-rank p50 = 50th value = 50, p90 = 90, max = 100.
        let mut v: Vec<u64> = (1..=100).collect();
        let s = summarize(&mut v);
        assert_eq!(s.p50, 50);
        assert_eq!(s.p90, 90);
        assert_eq!(s.max, 100);
        // 100 samples is below both floors — quoting p99/p999 here
        // would be statistical fiction.
        assert_eq!(s.p99, None);
        assert_eq!(s.p999, None);
    }

    #[test]
    fn floors_gate_tail_percentiles() {
        let mut v: Vec<u64> = (1..=1000).collect();
        let s = summarize(&mut v);
        assert_eq!(s.p99, Some(990));
        assert_eq!(s.p999, None, "p999 needs n >= 10000");

        let mut v: Vec<u64> = (1..=10_000).collect();
        let s = summarize(&mut v);
        assert_eq!(s.p99, Some(9900));
        assert_eq!(s.p999, Some(9990));
    }

    #[test]
    fn single_sample_is_all_percentiles() {
        let mut v = vec![42];
        let s = summarize(&mut v);
        assert_eq!((s.p50, s.p90, s.max), (42, 42, 42));
    }
}
