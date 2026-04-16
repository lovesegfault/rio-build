//! ADR-023 §Hardware heterogeneity: per-pod self-calibration microbench.
//!
//! Runs once at builder init (before accepting any assignment) and
//! reports `(hw_class, pod_id, factor)` to `hw_perf_samples` via the
//! store's `AppendHwPerfSample` RPC. The scheduler's `HwTable` reads
//! the per-hw_class median to map wall-seconds → reference-seconds
//! before fitting T(c).
//!
//! # Why CRC32 over a 64 KiB buffer
//!
//! Single-threaded, fixed-instruction-count, ~5 s on the slowest
//! admitted hw_class. CRC32 is compile-representative-ish (lexer/hash
//! inner loops are similar bit-twiddling + sequential memory) without
//! pulling in a real compiler. The 64 KiB buffer fits L1 on every
//! target so the result is dominated by core throughput, not memory
//! bandwidth — memory is NOT normalized (ADR-023: M(c) is fitted on
//! raw bytes).
//!
//! The factor is `REF_TIME_SECS / measured`: faster hardware → higher
//! factor → `wall × factor` stretches a fast-hw sample's wall-clock
//! UP to the reference timeline.

/// Inner-loop iteration count. Calibrated so [`REF_TIME_SECS`] ≈
/// measured on the slowest admitted hw_class (gen-5 c-category EBS).
const ITERS: usize = 50_000;

/// Wall-clock the bench takes on the reference (slowest admitted)
/// hw_class. `factor = REF_TIME_SECS / measured`, so the reference
/// class itself reports `factor ≈ 1.0` and faster classes report >1.
const REF_TIME_SECS: f64 = 5.0;

/// Run the microbench. Returns `factor = REF_TIME_SECS / measured`.
///
/// CPU-bound and blocking — call from `spawn_blocking`. Deterministic
/// instruction count; the only variable is the host's single-thread
/// throughput (which is exactly what we're measuring).
// r[impl sched.sla.hw-bench-append-only]
pub fn run() -> f64 {
    let buf = vec![0x5Au8; 65536];
    let start = std::time::Instant::now();
    let mut acc = 0u32;
    for _ in 0..ITERS {
        acc = acc.wrapping_add(crc32fast::hash(&buf));
    }
    // Prevent the optimizer from eliding the loop. crc32fast::hash is
    // pure on a constant buffer; without the sink the whole thing
    // folds to a constant.
    std::hint::black_box(acc);
    REF_TIME_SECS / start.elapsed().as_secs_f64()
}

/// Best-effort: run the bench and append the sample. Logs and returns
/// on any failure — a missed sample degrades normalization (hw_class
/// stays at `factor=1.0` until ≥3 distinct pods report), not the
/// build itself.
///
/// `hw_class` is the `rio.build/hw-class` pod annotation (controller-
/// stamped from the Node informer; downward-API → `RIO_HW_CLASS`).
/// Empty → skip: the controller hasn't stamped this pod yet (race at
/// pod-start), or the deployment isn't k8s. Either way, no hw_class
/// means the row would be useless.
pub async fn report(
    store: &mut rio_proto::StoreServiceClient<tonic::transport::Channel>,
    hw_class: &str,
    pod_id: &str,
) {
    if hw_class.is_empty() {
        tracing::debug!("hw_bench: no hw_class (RIO_HW_CLASS unset); skipping");
        return;
    }
    // spawn_blocking: ~5s of pure CPU. Running it inline on the
    // runtime would stall the executor's other startup tasks (FUSE
    // mount, heartbeat) for the duration.
    let factor = match tokio::task::spawn_blocking(run).await {
        Ok(f) => f,
        Err(e) => {
            tracing::warn!(error = %e, "hw_bench: spawn_blocking panicked");
            return;
        }
    };
    tracing::info!(%hw_class, %pod_id, factor, "hw_bench: measured");
    if let Err(e) = store
        .append_hw_perf_sample(rio_proto::types::AppendHwPerfSampleRequest {
            hw_class: hw_class.to_owned(),
            pod_id: pod_id.to_owned(),
            factor,
        })
        .await
    {
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
    /// calls on the same host should agree within ±5%. Also bounds the
    /// factor to `(0.1, 100.0)`: anything outside means either the loop
    /// was elided (factor → ∞) or `ITERS` drifted so far from
    /// `REF_TIME_SECS` calibration that the hw-normalize math is
    /// meaningless. The append-only contract relies on factors being
    /// comparable across pods; a non-reproducible bench would poison
    /// the `hw_perf_factors` median.
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
            rel < 0.05,
            "factor not reproducible: f1={f1} f2={f2} (rel diff {rel:.3})"
        );
    }
}
