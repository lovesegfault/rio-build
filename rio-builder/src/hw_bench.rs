//! ADR-023 §Hardware heterogeneity: per-pod self-calibration microbench.
//!
//! Runs once at builder init (before accepting any assignment) and
//! reports `(hw_class, pod_id, factor)` to `hw_perf_samples` via the
//! store's `AppendHwPerfSample` RPC. The scheduler's `HwTable` reads
//! the per-hw_class median to map wall-seconds → reference-seconds
//! before fitting T(c).
//!
//! # Why pointer-chasing over a 256 KiB buffer
//!
//! Single-threaded, fixed-iteration-count, ~5 s on the slowest
//! admitted hw_class. The previous CRC32 loop was a single hardware
//! instruction (`crc32` since Nehalem / ARMv8-CRC) — every admitted
//! hw_class retired it at 1/cycle so the bench measured base clock,
//! not the IPC + branch-predictor + L1/L2 latency that dominate a
//! compiler's hot path. Pointer-chasing a randomly-permuted ring over
//! a 256 KiB working set (L2-resident; defeats the stride prefetcher)
//! exercises exactly those: each load's address depends on the
//! previous load's result, so OoO width and L2 latency both show up.
//! 256 KiB stays inside L2 on every target so the result is still
//! core-dominated, not DRAM-bandwidth — memory is NOT normalized
//! (ADR-023: M(c) is fitted on raw bytes).
//!
//! The factor is `REF_TIME_SECS / measured`: faster hardware → higher
//! factor → `wall × factor` stretches a fast-hw sample's wall-clock
//! UP to the reference timeline.

/// 256 KiB working set as `u32` indices. L2-resident on every admitted
/// hw_class (≥512 KiB L2); large enough to spill L1 (32-64 KiB) so the
/// chase measures L2 latency, not L1.
const RING_LEN: usize = 65_536; // × 4 bytes = 256 KiB

/// Outer-loop iteration count. Calibrated so [`REF_TIME_SECS`] ≈
/// measured on the slowest admitted hw_class (gen-5 c-category EBS).
const ITERS: usize = 1_500;

/// Wall-clock the bench takes on the reference (slowest admitted)
/// hw_class. `factor = REF_TIME_SECS / measured`, so the reference
/// class itself reports `factor ≈ 1.0` and faster classes report >1.
const REF_TIME_SECS: f64 = 5.0;

/// Build a single-cycle random permutation over `0..RING_LEN`. Sattolo
/// shuffle (Fisher-Yates with `j < i` instead of `j ≤ i`) guarantees
/// one full cycle so the chase visits every slot before repeating —
/// no short loops that would fit L1. Fixed-seed xorshift so the
/// permutation (hence the instruction trace) is identical across pods.
fn build_ring() -> Vec<u32> {
    let mut ring: Vec<u32> = (0..RING_LEN as u32).collect();
    let mut s = 0x2545_F491_4F6C_DD1Du64;
    let mut next = || {
        s ^= s << 13;
        s ^= s >> 7;
        s ^= s << 17;
        s
    };
    for i in (1..RING_LEN).rev() {
        let j = (next() % i as u64) as usize;
        ring.swap(i, j);
    }
    ring
}

/// Run the microbench. Returns `factor = REF_TIME_SECS / measured`.
///
/// CPU-bound and blocking — call from `spawn_blocking`. Deterministic
/// instruction count; the only variable is the host's single-thread
/// throughput (which is exactly what we're measuring).
// r[impl sched.sla.hw-bench-append-only]
pub fn run() -> f64 {
    let ring = build_ring();
    let start = std::time::Instant::now();
    let mut idx = 0u32;
    for _ in 0..ITERS {
        // One full lap of the ring per outer iteration. The data
        // dependency `idx = ring[idx]` serializes the loads — the
        // prefetcher can't run ahead, and the branch predictor sees a
        // fixed-trip-count inner loop (compile-representative).
        for _ in 0..RING_LEN {
            idx = ring[idx as usize];
        }
        // Prevent the optimizer from hoisting/folding across outer
        // iterations.
        std::hint::black_box(idx);
    }
    std::hint::black_box(idx);
    REF_TIME_SECS / start.elapsed().as_secs_f64()
}

/// Spawn the ~5s CPU-bound microbench on a blocking thread. Called
/// from the resolve→bench task spawned at init (`runtime/setup.rs`),
/// which itself runs concurrently with FUSE mount + cold-start (~30s);
/// the result is consumed once the first `WorkAssignment` (and its
/// assignment token) is in hand.
///
/// `hw_class` is the `rio.build/hw-class` pod annotation, resolved
/// via [`crate::hw_class::resolve`] (downward-API volume + bounded
/// poll). Empty → `None`: the resolver expired (annotator dead) or
/// the deployment isn't k8s. Either way, no hw_class means the row
/// would be useless.
pub fn spawn_measure(hw_class: &str) -> Option<tokio::task::JoinHandle<f64>> {
    if hw_class.is_empty() {
        tracing::debug!("hw_bench: no hw_class (downward volume empty); skipping");
        return None;
    }
    // spawn_blocking: ~5s of pure CPU. Running it inline on the
    // runtime would stall the executor's other startup tasks (FUSE
    // mount, heartbeat) for the duration.
    Some(tokio::task::spawn_blocking(run))
}

/// Best-effort: append the bench sample. Logs and returns on any
/// failure — a missed sample degrades normalization (hw_class stays at
/// `factor=1.0` until ≥3 distinct pods report), not the build itself.
///
/// `factor` is the already-awaited result of [`spawn_measure`]; the
/// caller (`spawn_build_task`) awaits the spawned resolve→bench task
/// and passes the `f64` here.
///
/// **Why deferred until an assignment token is in hand:** the store
/// gates `AppendHwPerfSample` on `x-rio-assignment-token` and derives
/// `pod_id` from the token's `executor_id` claim
/// (`r[sec.boundary.grpc-hmac]`). The body `pod_id` is sent for
/// dev-mode fallback only.
pub async fn send(
    store: &mut rio_proto::StoreServiceClient<tonic::transport::Channel>,
    hw_class: &str,
    pod_id: &str,
    factor: f64,
    assignment_token: &str,
) {
    tracing::info!(%hw_class, %pod_id, factor, "hw_bench: measured");
    let mut req = tonic::Request::new(rio_proto::types::AppendHwPerfSampleRequest {
        hw_class: hw_class.to_owned(),
        pod_id: pod_id.to_owned(),
        factor,
    });
    if let Err(e) = crate::upload::common::attach_assignment_token(&mut req, assignment_token) {
        tracing::warn!(error = %e, "hw_bench: attach_assignment_token failed (best-effort)");
        return;
    }
    if let Err(e) = store.append_hw_perf_sample(req).await {
        tracing::warn!(error = %e, "hw_bench: AppendHwPerfSample failed (best-effort)");
    }
}

#[cfg(test)]
mod tests {
    /// The bench must produce a finite positive factor. We can't assert
    /// a specific value (host-dependent), but `Instant::elapsed` is
    /// monotonic-nonzero so `REF_TIME_SECS / elapsed` is finite-positive
    /// on any real host. This catches the optimizer-elision regression
    /// (loop folds → elapsed ≈ 0 → factor = ∞).
    #[test]
    fn run_produces_finite_positive_factor() {
        let f = super::run();
        assert!(f.is_finite(), "factor={f} (loop elided?)");
        assert!(f > 0.0, "factor={f}");
    }

    /// `run()` is deterministic-instruction-count → two back-to-back
    /// calls on the same host should agree within ±20%. Also bounds the
    /// factor to `(0.1, 100.0)`: anything outside means either the loop
    /// was elided (factor → ∞) or `ITERS` drifted so far from
    /// `REF_TIME_SECS` calibration that the hw-normalize math is
    /// meaningless. The append-only contract relies on factors being
    /// comparable across pods; a non-reproducible bench would poison
    /// the `hw_perf_factors` median.
    ///
    /// `threads-required = num-cpus` in `.config/nextest.toml` so this
    /// gets the machine to itself — the band was ±5% but flaked under
    /// parallel nextest load (contended L2 + DVFS); ±20% is the
    /// catches-elision-and-miscalibration band, not the production
    /// reproducibility band (the real bench runs on an idle pod at
    /// init, before any build).
    // r[verify sched.sla.hw-bench-append-only]
    #[test]
    fn run_is_reproducible_within_band() {
        let f1 = super::run();
        let f2 = super::run();
        assert!(
            f1.is_finite() && (0.1..100.0).contains(&f1),
            "f1={f1} outside (0.1, 100.0) — loop elided or ITERS miscalibrated"
        );
        assert!(
            f2.is_finite() && (0.1..100.0).contains(&f2),
            "f2={f2} outside (0.1, 100.0)"
        );
        let rel = (f1 - f2).abs() / f1.max(f2);
        assert!(
            rel < 0.20,
            "factor not reproducible: f1={f1} f2={f2} (rel diff {rel:.3})"
        );
    }

    /// Fixed-seed Sattolo: ring is one full cycle (visits every slot
    /// exactly once before returning to start). A short cycle would
    /// fit L1 and the bench would degenerate back to measuring base
    /// clock.
    #[test]
    fn ring_is_single_full_cycle() {
        let ring = super::build_ring();
        let mut idx = 0u32;
        for _ in 0..super::RING_LEN {
            idx = ring[idx as usize];
        }
        assert_eq!(idx, 0, "RING_LEN hops returns to start");
        // Halfway is NOT back at start (rules out a 2-cycle).
        let mut idx = 0u32;
        for _ in 0..super::RING_LEN / 2 {
            idx = ring[idx as usize];
        }
        assert_ne!(idx, 0);
    }
}
