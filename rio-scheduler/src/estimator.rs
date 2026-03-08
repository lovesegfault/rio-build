//! Build duration estimation from historical data.
//!
//! Feeds critical-path priority (D4) and size-class routing (D7).
//! The estimate is a HINT — a wrong estimate means suboptimal scheduling
//! (short job waits behind long job), not incorrectness. So we err on the
// r[impl sched.estimate.fallback-chain]
// r[impl sched.estimate.ema-alpha]
//! side of "always return SOMETHING" rather than "fail if uncertain".
//!
//! # Fallback chain (scheduler.md:105-110)
//!
//! 1. Exact `(pname, system)` match → EMA from `build_history`
//! 2. `pname` match on ANY system → cross-system EMA (ARM and x86 builds
//!    of the same package usually take similar time)
//! 3. Default: 30 seconds (configurable constant)
//!
//! Closure-size-as-proxy (scheduler.md fallback 2): now wired via
//! `input_srcs_nar_size` from the gateway (J1+J2). "A derivation
//! with 10GB of sources probably takes longer than one with 100MB."
//! Slots between the pname fallback and the 30s default.
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
    /// EMA of peak memory in bytes. `None` if no samples yet (e.g.,
    /// pre-seeded via psql with only duration, or an old build
    /// from before cgroup memory.peak was wired).
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
/// spec value (scheduler.md:110). Too short and critical-path priorities
/// are dominated by unknown derivations (everything looks equally urgent);
/// too long and unknown derivations hog the front of the queue.
///
/// 30s is roughly "a typical small build" — good enough until the first
/// real sample arrives.
pub const DEFAULT_DURATION_SECS: f64 = 30.0;

/// Rough "processing speed" for closure-size-as-proxy. 10 MB/s —
/// chosen as a deliberately CONSERVATIVE lower bound on "bytes
/// touched per second during a build." Real builds are faster (a
/// compiler reads source at disk speed then spends most time in
/// CPU), but we want the proxy to OVER-estimate, not under. An
/// over-estimate might give an unknown build higher priority than
/// it deserves (scheduled sooner); an under-estimate might put a
/// 30-minute build behind a 5-second one.
///
/// Tuned by observation: a 1GB source tarball (chromium-ish)
/// typically builds in ~100s minimum (unpack + configure + a
/// token compile). 1e9 / 1e7 = 100s. Checks out.
const CLOSURE_BYTES_PER_SEC_PROXY: f64 = 10_000_000.0;

/// Floor for the closure-size proxy. Even a tiny-srcs build has
/// to fork the builder, write $out. 5s is "the build actually
/// ran" vs the 30s default which is "we know literally nothing."
const CLOSURE_PROXY_MIN_SECS: f64 = 5.0;

impl Estimator {
    /// Estimate build duration in seconds.
    ///
    /// `pname` is `Option` because not every derivation has one (FODs,
    /// raw derivations without the stdenv `pname` attr). `None` skips
    /// the history lookups — there's nothing to key on. But the
    /// closure-size proxy (fallback #2.5) still applies: even a
    /// pname-less derivation has input_srcs.
    ///
    /// `system` is required (every derivation has one). But it only
    /// matters for fallback #1; fallback #2 ignores it.
    ///
    /// `input_srcs_nar_size` from the proto (J1). 0 = no-signal
    /// (empty srcs, or gateway's batch QueryPathInfo failed).
    pub fn estimate(&self, pname: Option<&str>, system: &str, input_srcs_nar_size: u64) -> f64 {
        // Fallbacks #1, #2: history-based. Require pname. If None,
        // skip to the closure-size proxy.
        if let Some(pname) = pname {
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

            // Fallback #2: same pname, any system. ARM and x86 builds
            // of gcc take roughly the same time — the algorithm
            // dominates, not the target.
            if let Some(&dur) = self.pname_fallback.get(pname) {
                return dur;
            }
        }

        // Fallback #2.5: closure-size-as-proxy. The "big source
        // tarball → probably long build" heuristic. Better than a
        // blind 30s when we at least know how many bytes the
        // builder will touch.
        //
        // Floor: even a 1-byte src doesn't estimate to 0.0001s.
        // 5s is the overhead floor (fork, write $out).
        //
        // Slot BETWEEN pname fallback and default: history is
        // always better (real data for THIS package); closure
        // size is better than nothing but it's a proxy.
        if input_srcs_nar_size > 0 {
            let raw = input_srcs_nar_size as f64 / CLOSURE_BYTES_PER_SEC_PROXY;
            return raw.max(CLOSURE_PROXY_MIN_SECS);
        }

        // Fallback #3: cold-start default. No history, no pname,
        // no srcs (or gateway batch failed). 30s.
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
    /// The input shape matches `SchedulerDb::read_build_history`
    /// exactly — callers just pipe the query result through.
    ///
    /// `_ema_cpu` is accepted but not stored — `HistoryEntry` doesn't
    /// have a cpu field yet. size-class routing currently bumps on
    /// MEMORY only (not cpu). When cpu-bump lands, adding the
    /// `HistoryEntry` field is a pure-estimator change; the DB
    /// roundtrip is already done.
    pub fn refresh(&mut self, rows: Vec<crate::db::BuildHistoryRow>) {
        let mut history = HashMap::with_capacity(rows.len());
        // pname → (sum, count) for mean calculation. Mean not median:
        // median would need sorting per-pname; mean is one pass. For
        // a fallback estimate, mean is plenty.
        let mut pname_acc: HashMap<String, (f64, u32)> = HashMap::new();

        for (pname, system, ema_duration, ema_mem, _ema_cpu) in rows {
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

// r[verify sched.estimate.fallback-chain]
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_returns_default() {
        let est = Estimator::default();
        assert_eq!(
            est.estimate(Some("gcc"), "x86_64-linux", 0),
            DEFAULT_DURATION_SECS
        );
        assert_eq!(est.estimate(None, "x86_64-linux", 0), DEFAULT_DURATION_SECS);
    }

    #[test]
    fn exact_match_wins() {
        let mut est = Estimator::default();
        est.refresh(vec![(
            "gcc".into(),
            "x86_64-linux".into(),
            120.0,
            None,
            None,
        )]);

        assert_eq!(est.estimate(Some("gcc"), "x86_64-linux", 0), 120.0);
    }

    #[test]
    fn pname_fallback_cross_system() {
        let mut est = Estimator::default();
        // Only aarch64 data; query for x86_64 → cross-system fallback.
        est.refresh(vec![(
            "gcc".into(),
            "aarch64-linux".into(),
            150.0,
            None,
            None,
        )]);

        assert_eq!(
            est.estimate(Some("gcc"), "x86_64-linux", 0),
            150.0,
            "no x86_64 data → use aarch64 EMA for same pname"
        );
    }

    #[test]
    fn pname_fallback_is_mean() {
        let mut est = Estimator::default();
        est.refresh(vec![
            ("gcc".into(), "x86_64-linux".into(), 100.0, None, None),
            ("gcc".into(), "aarch64-linux".into(), 200.0, None, None),
        ]);

        // Query for a THIRD system: fallback is mean(100, 200) = 150.
        assert_eq!(est.estimate(Some("gcc"), "riscv64-linux", 0), 150.0);
    }

    #[test]
    fn no_pname_skips_to_default() {
        let mut est = Estimator::default();
        est.refresh(vec![(
            "gcc".into(),
            "x86_64-linux".into(),
            120.0,
            None,
            None,
        )]);

        // pname=None → can't look anything up, even though history exists.
        assert_eq!(est.estimate(None, "x86_64-linux", 0), DEFAULT_DURATION_SECS);
    }

    #[test]
    fn unknown_pname_falls_through() {
        let mut est = Estimator::default();
        est.refresh(vec![(
            "gcc".into(),
            "x86_64-linux".into(),
            120.0,
            None,
            None,
        )]);

        assert_eq!(
            est.estimate(Some("unheard-of-package"), "x86_64-linux", 0),
            DEFAULT_DURATION_SECS
        );
    }

    #[test]
    fn peak_memory_lookup() {
        let mut est = Estimator::default();
        est.refresh(vec![
            (
                "firefox".into(),
                "x86_64-linux".into(),
                3600.0,
                Some(8e9),
                None,
            ),
            ("hello".into(), "x86_64-linux".into(), 5.0, None, None),
        ]);

        assert_eq!(est.peak_memory(Some("firefox"), "x86_64-linux"), Some(8e9));
        // No memory data for hello (seeded without ema_peak_memory_bytes).
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
            None,
        )]);

        // x86_64 lookup: no exact match → None (NOT the aarch64 value).
        assert_eq!(est.peak_memory(Some("firefox"), "x86_64-linux"), None);
    }

    #[test]
    fn refresh_replaces_not_merges() {
        let mut est = Estimator::default();
        est.refresh(vec![(
            "gcc".into(),
            "x86_64-linux".into(),
            100.0,
            None,
            None,
        )]);
        assert_eq!(est.estimate(Some("gcc"), "x86_64-linux", 0), 100.0);

        // Second refresh with DIFFERENT data: old entry gone.
        est.refresh(vec![(
            "clang".into(),
            "x86_64-linux".into(),
            80.0,
            None,
            None,
        )]);
        assert_eq!(est.estimate(Some("clang"), "x86_64-linux", 0), 80.0);
        // gcc no longer in history → default.
        assert_eq!(
            est.estimate(Some("gcc"), "x86_64-linux", 0),
            DEFAULT_DURATION_SECS
        );
    }

    #[test]
    fn len_and_is_empty() {
        let mut est = Estimator::default();
        assert!(est.is_empty());
        assert_eq!(est.len(), 0);

        est.refresh(vec![
            ("a".into(), "x".into(), 1.0, None, None),
            ("b".into(), "x".into(), 2.0, None, None),
        ]);
        assert!(!est.is_empty());
        assert_eq!(est.len(), 2);
    }

    // ---- J2: closure-size-as-proxy fallback (#2.5) ----

    #[test]
    fn closure_proxy_between_history_and_default() {
        let est = Estimator::default();
        // 1 GB of srcs, no history. 1e9 / 1e7 = 100s.
        let got = est.estimate(Some("unknown"), "x86_64-linux", 1_000_000_000);
        assert!(
            (got - 100.0).abs() < 0.001,
            "1GB / 10MB/s = 100s, got {got}"
        );
        // Check it's NOT the default — proves the proxy actually fires.
        assert_ne!(got, DEFAULT_DURATION_SECS);
    }

    #[test]
    fn closure_proxy_floor() {
        let est = Estimator::default();
        // Tiny srcs: 1 byte / 10MB/s = 0.0000001s. Floor kicks in.
        let got = est.estimate(Some("unknown"), "x86_64-linux", 1);
        assert_eq!(
            got, CLOSURE_PROXY_MIN_SECS,
            "even 1 byte of srcs → 5s floor (fork + write $out overhead)"
        );
    }

    #[test]
    fn closure_proxy_zero_is_no_signal() {
        let est = Estimator::default();
        // 0 = empty srcs OR gateway batch failed. Skip to default.
        let got = est.estimate(Some("unknown"), "x86_64-linux", 0);
        assert_eq!(
            got, DEFAULT_DURATION_SECS,
            "0 = no-signal → skip proxy, use 30s default"
        );
    }

    #[test]
    fn closure_proxy_lower_priority_than_history() {
        let mut est = Estimator::default();
        est.refresh(vec![(
            "gcc".into(),
            "x86_64-linux".into(),
            120.0,
            None,
            None,
        )]);
        // History says 120s. Pass a closure size that would compute
        // to 1000s. History WINS — it's real data for THIS package.
        let got = est.estimate(Some("gcc"), "x86_64-linux", 10_000_000_000);
        assert_eq!(
            got, 120.0,
            "history is real data; closure size is a proxy. History wins."
        );
    }

    #[test]
    fn closure_proxy_works_without_pname() {
        let est = Estimator::default();
        // pname=None skips history lookups but the proxy still
        // applies — even a pname-less derivation has srcs.
        let got = est.estimate(None, "x86_64-linux", 1_000_000_000);
        assert!(
            (got - 100.0).abs() < 0.001,
            "no pname, but 1GB srcs → 100s via proxy (not 30s default)"
        );
    }
}
