//! Build duration estimation from historical data.
//!
//! Feeds critical-path priority and size-class routing.
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
//! Closure-size-as-proxy (scheduler.md fallback 2): wired via
//! `input_srcs_nar_size` from the gateway. "A derivation with 10GB
//! of sources probably takes longer than one with 100MB."
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
    /// EMA of peak CPU cores. `None` if no samples yet. Feeds
    /// size-class cpu-bump (assignment::classify).
    pub ema_peak_cpu_cores: Option<f64>,
    /// `build_history.sample_count` — completions that have fed this
    /// EMA. Surfaced via `GetEstimatorStats` (I-124); the hot-path
    /// estimate doesn't use it, but "EMA after 1 sample" vs "after 50"
    /// matters to an operator reading `rio-cli estimator`.
    pub sample_count: i32,
}

/// Bucketed resource estimate for the capacity manifest (ADR-020).
/// Output of [`Estimator::bucketed_estimate`]; admin.rs converts to
/// the proto `DerivationResourceEstimate`. Local struct (not the
/// proto type) keeps estimator free of the rio-proto dependency.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BucketedEstimate {
    /// EMA × headroom, rounded UP to [`MEMORY_BUCKET_BYTES`].
    pub memory_bytes: u64,
    /// EMA × headroom × 1000, rounded UP to [`CPU_BUCKET_MILLICORES`].
    pub cpu_millicores: u32,
    /// EMA duration, truncated. Informational (controller MAY use
    /// for grace-period hints).
    pub duration_secs: u32,
}

/// Memory bucket granularity: 4 GiB. ADR-020 § Decision ¶2 — two
/// derivations at 6.2Gi and 7.8Gi both land in the 8Gi bucket →
/// share a pod sequentially. Without bucketing every derivation is
/// a unique (mem, cpu) pair and the controller would spawn N
/// single-use pods.
pub const MEMORY_BUCKET_BYTES: u64 = 4 * 1024 * 1024 * 1024;

/// CPU bucket granularity: 2000 millicores (2 cores). Millicores
/// because k8s ResourceRequirements speaks millicores — saves the
/// controller a ×1000.
pub const CPU_BUCKET_MILLICORES: u32 = 2000;

/// ADR-020 headroom default: EMA × 1.25 before bucketing. A build
/// that peaked at 6.2Gi EMA gets a 7.75Gi request → buckets to 8Gi.
/// 25% slack covers EMA lag (a burstier run than history suggests)
/// without over-provisioning. Shared by `DagActor::new` (dispatch-
/// time resource-fit filter) and main.rs's `default_headroom_
/// multiplier` (manifest RPC) so both see the SAME default — a
/// mismatch would make the controller spawn pods the filter rejects.
pub const DEFAULT_HEADROOM_MULTIPLIER: f64 = 1.25;

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
    /// `input_srcs_nar_size` from the proto. 0 = no-signal
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

    /// Look up peak memory for size-class bumping.
    ///
    /// `None` means no history OR no memory data (VmHWM not wired yet).
    /// Treated as "no bump" — duration-based classification only.
    pub fn peak_memory(&self, pname: Option<&str>, system: &str) -> Option<f64> {
        let pname = pname?;
        self.history
            .get(&(pname.to_string(), system.to_string()))
            .and_then(|e| e.ema_peak_memory_bytes)
    }

    /// Look up peak CPU for size-class cpu-bump. Mirrors [`peak_memory`].
    ///
    /// `None` means no history OR no cpu data. No cross-system fallback —
    /// same rationale as [`peak_memory`]: resource ceilings are
    /// architecture-dependent, better to say "unknown" than guess.
    ///
    /// [`peak_memory`]: Self::peak_memory
    pub fn peak_cpu(&self, pname: Option<&str>, system: &str) -> Option<f64> {
        let pname = pname?;
        self.history
            .get(&(pname.to_string(), system.to_string()))
            .and_then(|e| e.ema_peak_cpu_cores)
    }

    /// Look up the full [`HistoryEntry`] for a `(pname, system)`.
    ///
    /// `None` means no history. No cross-system fallback — same
    /// rationale as [`peak_memory`]: resource estimates are
    /// architecture-dependent, better to say "unknown" than guess.
    /// [`HistoryEntry`] is `Copy`; return by value avoids tying the
    /// caller's lifetime to the estimator.
    ///
    /// [`peak_memory`]: Self::peak_memory
    pub fn lookup_entry(&self, pname: &str, system: &str) -> Option<HistoryEntry> {
        self.history
            .get(&(pname.to_string(), system.to_string()))
            .copied()
    }

    /// Bucket a [`HistoryEntry`] for the capacity manifest (ADR-020).
    ///
    /// Applies `headroom_mult` then rounds UP: memory to 4GiB buckets,
    /// CPU to 2000-millicore buckets. Returns `None` if the entry has
    /// no memory sample (cold start — caller uses the operator floor,
    /// not a guess). If memory is present but CPU is absent (partial
    /// sample — old data from before cgroup cpu.stat was wired), CPU
    /// defaults to one bucket (2 cores).
    ///
    /// Rounding-at-source is load-bearing: the controller and any
    /// future consumers see the same buckets; two derivations that
    /// should share a pod don't diverge from f64 noise applied in
    /// different places.
    ///
    /// Not a `&self` method: takes a `HistoryEntry` directly so T3's
    /// handler can look up once and bucket, rather than re-looking-up
    /// by (pname, system) here.
    // r[impl sched.admin.capacity-manifest.bucket]
    pub fn bucketed_estimate(entry: &HistoryEntry, headroom_mult: f64) -> Option<BucketedEstimate> {
        let mem_ema = entry.ema_peak_memory_bytes?;

        // Memory: EMA × headroom, ceil to bucket. f64→u64 via ceil
        // then integer-ceil: (x + b - 1) / b * b. The f64 ceil
        // handles the headroom math (6.2e9 × 1.25 = 7.75e9); the
        // integer ceil handles the bucketing (7.75e9 → 8 GiB).
        // .max(bucket) guarantees nonzero even if EMA is pathological
        // (0.0 from a broken sample).
        let mem_raw = (mem_ema * headroom_mult).ceil() as u64;
        let memory_bytes = mem_raw.div_ceil(MEMORY_BUCKET_BYTES).max(1) * MEMORY_BUCKET_BYTES;

        // CPU: cores→millicores, same ceiling math. Absent CPU
        // defaults to 1.0 core BEFORE headroom — after ×1.25 that's
        // 1250mcores → buckets to 2000. A single-threaded build is
        // the conservative guess.
        let cpu_cores = entry.ema_peak_cpu_cores.unwrap_or(1.0);
        let cpu_raw = (cpu_cores * headroom_mult * 1000.0).ceil() as u32;
        let cpu_millicores = cpu_raw.div_ceil(CPU_BUCKET_MILLICORES).max(1) * CPU_BUCKET_MILLICORES;

        Some(BucketedEstimate {
            memory_bytes,
            cpu_millicores,
            duration_secs: entry.ema_duration_secs as u32,
        })
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
    pub fn refresh(&mut self, rows: Vec<crate::db::BuildHistoryRow>) {
        let mut history = HashMap::with_capacity(rows.len());
        // pname → (sum, count) for mean calculation. Mean not median:
        // median would need sorting per-pname; mean is one pass. For
        // a fallback estimate, mean is plenty.
        let mut pname_acc: HashMap<String, (f64, u32)> = HashMap::new();

        for (pname, system, ema_duration, ema_mem, ema_cpu, sample_count) in rows {
            let (sum, count) = pname_acc.entry(pname.clone()).or_insert((0.0, 0));
            *sum += ema_duration;
            *count += 1;

            history.insert(
                (pname, system),
                HistoryEntry {
                    ema_duration_secs: ema_duration,
                    ema_peak_memory_bytes: ema_mem,
                    ema_peak_cpu_cores: ema_cpu,
                    sample_count,
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

    /// Iterate the in-memory `(pname, system) → HistoryEntry` snapshot.
    /// Feeds `GetEstimatorStats` (I-124) — the actor walks this to
    /// classify each entry under the current effective cutoffs.
    pub fn iter_history(&self) -> impl Iterator<Item = (&(String, String), &HistoryEntry)> {
        self.history.iter()
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

    /// Shorthand for a `BuildHistoryRow` test fixture. `sample_count`
    /// defaults to 1 (irrelevant to the estimate fallback chain; only
    /// `iter_history` consumers care).
    fn row(
        pname: &str,
        system: &str,
        dur: f64,
        mem: Option<f64>,
        cpu: Option<f64>,
    ) -> crate::db::BuildHistoryRow {
        (pname.into(), system.into(), dur, mem, cpu, 1)
    }

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
        est.refresh(vec![row("gcc", "x86_64-linux", 120.0, None, None)]);

        assert_eq!(est.estimate(Some("gcc"), "x86_64-linux", 0), 120.0);
    }

    #[test]
    fn pname_fallback_cross_system() {
        let mut est = Estimator::default();
        // Only aarch64 data; query for x86_64 → cross-system fallback.
        est.refresh(vec![row("gcc", "aarch64-linux", 150.0, None, None)]);

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
            row("gcc", "x86_64-linux", 100.0, None, None),
            row("gcc", "aarch64-linux", 200.0, None, None),
        ]);

        // Query for a THIRD system: fallback is mean(100, 200) = 150.
        assert_eq!(est.estimate(Some("gcc"), "riscv64-linux", 0), 150.0);
    }

    #[test]
    fn no_pname_skips_to_default() {
        let mut est = Estimator::default();
        est.refresh(vec![row("gcc", "x86_64-linux", 120.0, None, None)]);

        // pname=None → can't look anything up, even though history exists.
        assert_eq!(est.estimate(None, "x86_64-linux", 0), DEFAULT_DURATION_SECS);
    }

    #[test]
    fn unknown_pname_falls_through() {
        let mut est = Estimator::default();
        est.refresh(vec![row("gcc", "x86_64-linux", 120.0, None, None)]);

        assert_eq!(
            est.estimate(Some("unheard-of-package"), "x86_64-linux", 0),
            DEFAULT_DURATION_SECS
        );
    }

    #[test]
    fn peak_memory_lookup() {
        let mut est = Estimator::default();
        est.refresh(vec![
            row("firefox", "x86_64-linux", 3600.0, Some(8e9), None),
            row("hello", "x86_64-linux", 5.0, None, None),
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
        est.refresh(vec![row(
            "firefox",
            "aarch64-linux",
            3600.0,
            Some(8e9),
            None,
        )]);

        // x86_64 lookup: no exact match → None (NOT the aarch64 value).
        assert_eq!(est.peak_memory(Some("firefox"), "x86_64-linux"), None);
    }

    #[test]
    fn peak_cpu_lookup() {
        let mut est = Estimator::default();
        est.refresh(vec![
            row("chromium", "x86_64-linux", 7200.0, None, Some(12.0)),
            row("hello", "x86_64-linux", 5.0, None, None),
        ]);

        assert_eq!(est.peak_cpu(Some("chromium"), "x86_64-linux"), Some(12.0));
        assert_eq!(est.peak_cpu(Some("hello"), "x86_64-linux"), None);
        assert_eq!(est.peak_cpu(Some("missing"), "x86_64-linux"), None);
        assert_eq!(est.peak_cpu(None, "x86_64-linux"), None);
        // No cross-system fallback (same rationale as peak_memory).
        assert_eq!(est.peak_cpu(Some("chromium"), "aarch64-linux"), None);
    }

    #[test]
    fn refresh_replaces_not_merges() {
        let mut est = Estimator::default();
        est.refresh(vec![row("gcc", "x86_64-linux", 100.0, None, None)]);
        assert_eq!(est.estimate(Some("gcc"), "x86_64-linux", 0), 100.0);

        // Second refresh with DIFFERENT data: old entry gone.
        est.refresh(vec![row("clang", "x86_64-linux", 80.0, None, None)]);
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
            row("a", "x", 1.0, None, None),
            row("b", "x", 2.0, None, None),
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
        est.refresh(vec![row("gcc", "x86_64-linux", 120.0, None, None)]);
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

    // ─── bucketed_estimate (ADR-020 P0501 T2) ───────────────────────

    const GIB: f64 = 1024.0 * 1024.0 * 1024.0;

    fn entry(mem_gib: Option<f64>, cpu_cores: Option<f64>, dur_secs: f64) -> HistoryEntry {
        HistoryEntry {
            ema_duration_secs: dur_secs,
            ema_peak_memory_bytes: mem_gib.map(|g| g * GIB),
            ema_peak_cpu_cores: cpu_cores,
            sample_count: 1,
        }
    }

    // r[verify sched.admin.capacity-manifest.bucket]
    #[test]
    fn bucket_rounds_memory_up() {
        // 6.2 GiB × 1.25 = 7.75 GiB → ceils to 8 GiB (2 buckets).
        // Not 4 GiB (too small), not 12 GiB (over-bucketed).
        let e = entry(Some(6.2), Some(2.0), 60.0);
        let b = Estimator::bucketed_estimate(&e, 1.25).unwrap();
        assert_eq!(b.memory_bytes, 2 * MEMORY_BUCKET_BYTES, "6.2Gi×1.25 → 8Gi");
    }

    #[test]
    fn bucket_rounds_cpu_up_to_minimum() {
        // 0.3 cores × 1.25 = 0.375 cores = 375 mcores → ceils to
        // 2000 (1 bucket). Minimum bucket is never underflowed.
        let e = entry(Some(4.0), Some(0.3), 30.0);
        let b = Estimator::bucketed_estimate(&e, 1.25).unwrap();
        assert_eq!(b.cpu_millicores, CPU_BUCKET_MILLICORES, "375mcores → 2000");
    }

    #[test]
    fn bucket_cold_start_returns_none() {
        // No memory sample → None. Controller uses the operator
        // floor, not a guess.
        let e = entry(None, Some(4.0), 60.0);
        assert_eq!(Estimator::bucketed_estimate(&e, 1.25), None);
    }

    #[test]
    fn bucket_cpu_absent_defaults_one_core() {
        // Memory present, CPU absent (old sample). Defaults to 1.0
        // core BEFORE headroom: 1.0 × 1.25 = 1250mcores → buckets
        // to 2000. Partial sample gets a conservative default, not
        // a None (memory IS real data).
        let e = entry(Some(4.0), None, 60.0);
        let b = Estimator::bucketed_estimate(&e, 1.25).unwrap();
        assert_eq!(b.cpu_millicores, CPU_BUCKET_MILLICORES);
        assert_eq!(b.memory_bytes, 2 * MEMORY_BUCKET_BYTES, "4Gi×1.25 → 8Gi");
    }

    #[test]
    fn bucket_both_cluster() {
        // Two derivations that should share a pod bucket to the
        // same (mem, cpu). This is the ADR-020 load-bearing case:
        // 6.2Gi and 7.8Gi both → 8Gi after ×1.25 headroom.
        let e1 = entry(Some(6.2), Some(1.8), 60.0);
        let e2 = entry(Some(5.9), Some(2.1), 90.0);
        let b1 = Estimator::bucketed_estimate(&e1, 1.25).unwrap();
        let b2 = Estimator::bucketed_estimate(&e2, 1.25).unwrap();
        assert_eq!(
            (b1.memory_bytes, b1.cpu_millicores),
            (b2.memory_bytes, b2.cpu_millicores),
            "6.2Gi/1.8c and 5.9Gi/2.1c both bucket to 8Gi/4000mc → share a pod"
        );
    }

    #[test]
    fn bucket_zero_memory_still_one_bucket() {
        // Pathological: EMA says 0 bytes (broken sample). Still
        // returns 1 bucket, not 0. .max(1) on the bucket count.
        let e = entry(Some(0.0), Some(1.0), 10.0);
        let b = Estimator::bucketed_estimate(&e, 1.25).unwrap();
        assert_eq!(b.memory_bytes, MEMORY_BUCKET_BYTES, "0 → 1 bucket floor");
    }

    #[test]
    fn bucket_large_memory_no_overflow() {
        // 200 GiB × 1.25 = 250 GiB. u64 handles this fine
        // (max ~18 EiB). Also verifies multi-bucket math.
        let e = entry(Some(200.0), Some(32.0), 3600.0);
        let b = Estimator::bucketed_estimate(&e, 1.25).unwrap();
        // 250 GiB / 4 GiB = 62.5 → ceils to 63 buckets = 252 GiB
        assert_eq!(b.memory_bytes, 63 * MEMORY_BUCKET_BYTES);
        // 32 × 1.25 = 40 cores = 40000mcores / 2000 = 20 buckets
        assert_eq!(b.cpu_millicores, 20 * CPU_BUCKET_MILLICORES);
        assert_eq!(b.duration_secs, 3600);
    }
}
