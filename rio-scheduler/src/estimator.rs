//! Build duration estimation from historical data.
//!
//! Feeds critical-path priority (D4) and size-class routing (D7).
//! The estimate is a HINT — a wrong estimate means suboptimal scheduling
//! (short job waits behind long job), not incorrectness. So we err on the
//! side of "always return SOMETHING" rather than "fail if uncertain".
//!
//! # Fallback chain (scheduler.md:97-102)
//!
//! 1. Exact `(pname, system)` match → EMA from `build_history`
//! 2. `pname` match on ANY system → cross-system EMA (ARM and x86 builds
//!    of the same package usually take similar time)
//! 3. Default: 30 seconds (configurable constant)
//!
//! Closure-size-as-proxy (scheduler.md fallback 2) is deferred —
//! `TODO(phase3a)`: needs per-path nar_size which the scheduler doesn't
//! currently track. The 30s default covers the cold-start case adequately.
//!
//! # Refresh, not live
//!
//! The estimator is a snapshot loaded from PG. It's refreshed periodically
//! (Tick, ~60s) but NOT on every `update_build_history` write. That's fine:
//! estimates don't need to be real-time; a build completing doesn't change
//! the critical path of ALREADY-QUEUED derivations (they were prioritized
//! based on the estimate at merge time). The refresh catches up for NEW
//! submissions.

use std::collections::HashMap;

/// Historical EMA data for one (pname, system).
#[derive(Debug, Clone, Copy)]
pub struct HistoryEntry {
    /// EMA of build duration in seconds.
    pub ema_duration_secs: f64,
    /// EMA of peak memory in bytes. `None` until F2 wires VmHWM reporting.
    pub ema_peak_memory_bytes: Option<f64>,
}

/// Duration estimator. Owned by the actor (single-threaded; no lock).
///
/// `Default` gives an empty estimator — all queries return the fallback
/// constant. The actor populates it via `refresh()` on startup and on Tick.
#[derive(Debug, Default)]
pub struct Estimator {
    /// (pname, system) → history. The primary lookup.
    history: HashMap<(String, String), HistoryEntry>,
    /// pname → any-system EMA duration. For fallback #2. Built from
    /// `history` at refresh time (not queried separately) — same rows,
    /// aggregated by pname. If a pname has entries for multiple systems,
    /// we take the MEAN (not max, not min; no system is privileged).
    pname_fallback: HashMap<String, f64>,
}

/// Default duration estimate when we have no history. 30 seconds is the
/// spec value (scheduler.md:102). Too short and critical-path priorities
/// are dominated by unknown derivations (everything looks equally urgent);
/// too long and unknown derivations hog the front of the queue.
///
/// 30s is roughly "a typical small build" — good enough until the first
/// real sample arrives.
pub const DEFAULT_DURATION_SECS: f64 = 30.0;

impl Estimator {
    /// Estimate build duration in seconds.
    ///
    /// `pname` is `Option` because not every derivation has one (FODs,
    /// raw derivations without the stdenv `pname` attr). `None` skips
    /// straight to the default — there's nothing to key a lookup on.
    ///
    /// `system` is required (every derivation has one). But it only
    /// matters for fallback #1; fallback #2 ignores it.
    pub fn estimate(&self, pname: Option<&str>, system: &str) -> f64 {
        let Some(pname) = pname else {
            // No pname = no lookup key. Default.
            return DEFAULT_DURATION_SECS;
        };

        // Fallback #1: exact (pname, system).
        //
        // The tuple-key lookup allocates two Strings. For a hot path
        // (called once per dispatch decision, ~1000/sec under load),
        // that's ~2k small allocs/sec — measurable but not dominant.
        // A Cow-based key or string-interning would fix it; deferred
        // until profiling says it matters.
        if let Some(entry) = self.history.get(&(pname.to_string(), system.to_string())) {
            return entry.ema_duration_secs;
        }

        // Fallback #2: same pname, any system. ARM and x86 builds of
        // gcc take roughly the same time — the algorithm dominates,
        // not the target.
        if let Some(&dur) = self.pname_fallback.get(pname) {
            return dur;
        }

        // Fallback #3: cold-start default.
        //
        // TODO(phase3a): closure-size-as-proxy. A derivation with a
        // 10 GB closure probably takes longer than one with a 100 MB
        // closure — use the sum of input nar_sizes as a rough signal.
        // Needs the scheduler to track nar_size per path, which it
        // doesn't yet. The 30s default is acceptable until then.
        DEFAULT_DURATION_SECS
    }

    /// Look up peak memory for size-class bumping (D7).
    ///
    /// `None` means no history OR no memory data (VmHWM not wired yet).
    /// D7 treats `None` as "no bump" — duration-based classification only.
    pub fn peak_memory(&self, pname: Option<&str>, system: &str) -> Option<f64> {
        let pname = pname?;
        self.history
            .get(&(pname.to_string(), system.to_string()))
            .and_then(|e| e.ema_peak_memory_bytes)
    }

    /// Rebuild from a fresh `build_history` read.
    ///
    /// Called by the actor on Tick (every ~60s). Replaces the whole
    /// state — no incremental update. The rows are small (pname +
    /// system + two f64s, ~80 bytes each); 10k rows = 800 KB. Full
    /// replace is simpler than diffing and fast enough.
    ///
    /// The input tuple shape matches `SchedulerDb::read_build_history`
    /// exactly — callers just pipe the query result through.
    pub fn refresh(&mut self, rows: Vec<(String, String, f64, Option<f64>)>) {
        let mut history = HashMap::with_capacity(rows.len());
        // pname → (sum, count) for mean calculation. Mean not median:
        // median would need sorting per-pname; mean is one pass. For
        // a fallback estimate, mean is plenty.
        let mut pname_acc: HashMap<String, (f64, u32)> = HashMap::new();

        for (pname, system, ema_duration, ema_mem) in rows {
            let (sum, count) = pname_acc.entry(pname.clone()).or_insert((0.0, 0));
            *sum += ema_duration;
            *count += 1;

            history.insert(
                (pname, system),
                HistoryEntry {
                    ema_duration_secs: ema_duration,
                    ema_peak_memory_bytes: ema_mem,
                },
            );
        }

        let pname_fallback = pname_acc
            .into_iter()
            .map(|(pname, (sum, count))| (pname, sum / count as f64))
            .collect();

        self.history = history;
        self.pname_fallback = pname_fallback;
    }

    /// Number of (pname, system) entries. Test-only.
    #[cfg(test)]
    pub fn len(&self) -> usize {
        self.history.len()
    }

    #[cfg(test)]
    pub fn is_empty(&self) -> bool {
        self.history.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_returns_default() {
        let est = Estimator::default();
        assert_eq!(
            est.estimate(Some("gcc"), "x86_64-linux"),
            DEFAULT_DURATION_SECS
        );
        assert_eq!(est.estimate(None, "x86_64-linux"), DEFAULT_DURATION_SECS);
    }

    #[test]
    fn exact_match_wins() {
        let mut est = Estimator::default();
        est.refresh(vec![("gcc".into(), "x86_64-linux".into(), 120.0, None)]);

        assert_eq!(est.estimate(Some("gcc"), "x86_64-linux"), 120.0);
    }

    #[test]
    fn pname_fallback_cross_system() {
        let mut est = Estimator::default();
        // Only aarch64 data; query for x86_64 → cross-system fallback.
        est.refresh(vec![("gcc".into(), "aarch64-linux".into(), 150.0, None)]);

        assert_eq!(
            est.estimate(Some("gcc"), "x86_64-linux"),
            150.0,
            "no x86_64 data → use aarch64 EMA for same pname"
        );
    }

    #[test]
    fn pname_fallback_is_mean() {
        let mut est = Estimator::default();
        est.refresh(vec![
            ("gcc".into(), "x86_64-linux".into(), 100.0, None),
            ("gcc".into(), "aarch64-linux".into(), 200.0, None),
        ]);

        // Query for a THIRD system: fallback is mean(100, 200) = 150.
        assert_eq!(est.estimate(Some("gcc"), "riscv64-linux"), 150.0);
    }

    #[test]
    fn no_pname_skips_to_default() {
        let mut est = Estimator::default();
        est.refresh(vec![("gcc".into(), "x86_64-linux".into(), 120.0, None)]);

        // pname=None → can't look anything up, even though history exists.
        assert_eq!(est.estimate(None, "x86_64-linux"), DEFAULT_DURATION_SECS);
    }

    #[test]
    fn unknown_pname_falls_through() {
        let mut est = Estimator::default();
        est.refresh(vec![("gcc".into(), "x86_64-linux".into(), 120.0, None)]);

        assert_eq!(
            est.estimate(Some("unheard-of-package"), "x86_64-linux"),
            DEFAULT_DURATION_SECS
        );
    }

    #[test]
    fn peak_memory_lookup() {
        let mut est = Estimator::default();
        est.refresh(vec![
            ("firefox".into(), "x86_64-linux".into(), 3600.0, Some(8e9)),
            ("hello".into(), "x86_64-linux".into(), 5.0, None),
        ]);

        assert_eq!(est.peak_memory(Some("firefox"), "x86_64-linux"), Some(8e9));
        // No memory data for hello (VmHWM not wired / build was from before F2).
        assert_eq!(est.peak_memory(Some("hello"), "x86_64-linux"), None);
        // Unknown → None.
        assert_eq!(est.peak_memory(Some("missing"), "x86_64-linux"), None);
        // No pname → None.
        assert_eq!(est.peak_memory(None, "x86_64-linux"), None);
    }

    /// peak_memory does NOT do cross-system fallback. Memory is more
    /// architecture-dependent than duration (different word sizes,
    /// different optimization passes). We'd rather say "unknown" than
    /// guess wrong and OOM-kill on a small worker.
    #[test]
    fn peak_memory_no_cross_system_fallback() {
        let mut est = Estimator::default();
        est.refresh(vec![(
            "firefox".into(),
            "aarch64-linux".into(),
            3600.0,
            Some(8e9),
        )]);

        // x86_64 lookup: no exact match → None (NOT the aarch64 value).
        assert_eq!(est.peak_memory(Some("firefox"), "x86_64-linux"), None);
    }

    #[test]
    fn refresh_replaces_not_merges() {
        let mut est = Estimator::default();
        est.refresh(vec![("gcc".into(), "x86_64-linux".into(), 100.0, None)]);
        assert_eq!(est.estimate(Some("gcc"), "x86_64-linux"), 100.0);

        // Second refresh with DIFFERENT data: old entry gone.
        est.refresh(vec![("clang".into(), "x86_64-linux".into(), 80.0, None)]);
        assert_eq!(est.estimate(Some("clang"), "x86_64-linux"), 80.0);
        // gcc no longer in history → default.
        assert_eq!(
            est.estimate(Some("gcc"), "x86_64-linux"),
            DEFAULT_DURATION_SECS
        );
    }

    #[test]
    fn len_and_is_empty() {
        let mut est = Estimator::default();
        assert!(est.is_empty());
        assert_eq!(est.len(), 0);

        est.refresh(vec![
            ("a".into(), "x".into(), 1.0, None),
            ("b".into(), "x".into(), 2.0, None),
        ]);
        assert!(!est.is_empty());
        assert_eq!(est.len(), 2);
    }
}
