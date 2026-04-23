//! ADR-023 §Hardware heterogeneity: K=3 self-calibration microbench.
//!
//! Runs once at builder init (before accepting any assignment) and
//! reports `(hw_class, pod_id, factor jsonb, submitting_tenant)` to
//! `hw_perf_samples` via the store's `AppendHwPerfSample` RPC. The
//! scheduler's `HwTable` per-dimension median maps wall-seconds →
//! reference-seconds before fitting T(c).
//!
//! # K=3 vector: `[alu, membw, ioseq]`
//!
//! `factor[d] = observed_throughput[d] / REF_throughput[d]`, so
//! faster hardware → higher factor → `wall × (α · factor)` stretches a
//! fast-hw sample's wall-clock UP to the reference timeline. The K=3
//! basis matches the axes cloud instance families segment on:
//! gen-n→n+1 improves `alu`, `r`/`x` families improve `membw`, and
//! `*d`/`i*` families (instance-store NVMe) improve `ioseq`.
//!
//! # Probe schedule
//!
//! `alu` and `ioseq` run **concurrently** (~5 s each); `membw` (~3 s)
//! runs **alone after**, since STREAM triad saturates the memory bus
//! and would contaminate the `alu` pointer-chase. Total ~8 s.
//!
//! # Gating
//!
//! The full K=3 bench runs only when the controller stamps
//! `rio.build/hw-bench-needed=true` on the pod (downward-API →
//! `RIO_HW_BENCH_NEEDED`). Fail-closed: when unset only the scalar
//! `alu` probe runs and `membw`/`ioseq` report `1.0`, so with
//! default-uniform α the model degrades to scalar.

use std::alloc::Layout;
use std::hint::black_box;
use std::io::{Seek, SeekFrom, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::time::Instant;

/// Index order matches the jsonb keys and the per-pname mixture α.
pub const K: usize = 3;
pub const ALU: usize = 0;
pub const MEMBW: usize = 1;
pub const IOSEQ: usize = 2;

// ─── alu probe (pointer-chase) ───────────────────────────────────────

/// 256 KiB working set as `u32` indices. L2-resident on every admitted
/// hw_class (≥512 KiB L2); large enough to spill L1 (32-64 KiB) so the
/// chase measures L2 latency, not L1.
const RING_LEN: usize = 65_536; // × 4 bytes = 256 KiB

/// Outer-loop iteration count. Calibrated so [`REF_ALU_SECS`] ≈
/// measured on the slowest admitted hw_class (gen-5 c-category EBS).
const ITERS: usize = 1_500;

/// Wall-clock the alu bench takes on the reference (slowest admitted)
/// hw_class. `factor[ALU] = REF_ALU_SECS / measured`, so the reference
/// class itself reports `factor ≈ 1.0` and faster classes report >1.
const REF_ALU_SECS: f64 = 5.0;

/// STREAM triad GB/s on the reference hw_class (single-thread).
const REF_MEMBW_GBPS: f64 = 12.0;

/// `O_DIRECT|O_DSYNC` 1 MiB seq-write MB/s on the reference hw_class
/// (gp3 root volume at default 125 MB/s provisioned throughput).
const REF_IOSEQ_MBPS: f64 = 125.0;

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

/// `alu` probe: pointer-chase a Sattolo ring in L2. Returns
/// `factor[ALU] = REF_ALU_SECS / measured`.
///
/// Why pointer-chasing over a 256 KiB buffer: the previous CRC32 loop
/// was a single hardware instruction (`crc32` since Nehalem /
/// ARMv8-CRC) — every admitted hw_class retired it at 1/cycle so the
/// bench measured base clock, not the IPC + branch-predictor + L1/L2
/// latency that dominate a compiler's hot path. Each load's address
/// here depends on the previous load's result, so OoO width and L2
/// latency both show up. 256 KiB stays inside L2 on every target so
/// the result is core-dominated, not DRAM-bandwidth — DRAM is
/// `membw`'s job.
///
/// CPU-bound and blocking — call from `spawn_blocking`.
// r[impl sched.sla.hw-bench-append-only]
pub fn alu_factor() -> f64 {
    let ring = build_ring();
    let start = Instant::now();
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
        black_box(idx);
    }
    black_box(idx);
    REF_ALU_SECS / start.elapsed().as_secs_f64()
}

// ─── membw probe (STREAM triad) ──────────────────────────────────────

/// STREAM triad: `a[i] = b[i] + scalar * c[i]`. Returns sustained GB/s.
///
/// Each array is `bytes` long; total working set 3×`bytes`. Caller
/// picks `bytes` ≥ 4×LLC (ADR-023: 1.5 GiB on c7a.48xlarge). Single-
/// threaded by design — per-core memory bandwidth, not socket
/// aggregate. Iteration count targets ~3 s on the reference class.
///
/// Allocations are freed on return; [`spawn_measure`] runs this AFTER
/// the alu+ioseq pair, alone, so the ~4.6 GiB working set doesn't
/// contend with the build.
pub fn stream_triad_gbps(bytes: usize) -> f64 {
    let n = bytes / 8;
    let mut a = vec![1.0f64; n];
    let b = vec![2.0f64; n];
    let c = vec![0.5f64; n];
    let scalar = 3.0f64;
    // Warmup: touch every page so the timed loop sees no first-touch
    // page faults.
    for i in 0..n {
        a[i] = b[i] + scalar * c[i];
    }
    black_box(&mut a);
    // Target ~3 s of traffic at ~REF_MEMBW_GBPS, floored at 4 iters so
    // a tiny test `bytes` still produces a stable median.
    let iters = ((3.0 * REF_MEMBW_GBPS * 1e9) as usize / (3 * bytes)).max(4);
    let start = Instant::now();
    for _ in 0..iters {
        for i in 0..n {
            a[i] = b[i] + scalar * c[i];
        }
        black_box(&mut a);
    }
    let elapsed = start.elapsed().as_secs_f64();
    // Triad touches 3 arrays per iter (2 read + 1 write); STREAM
    // convention counts user-visible traffic, not the RFO write-miss.
    (3.0 * bytes as f64 * iters as f64) / elapsed / 1e9
}

/// Per-array bytes for [`stream_triad_gbps`]. Prod default 1.5 GiB
/// (4× the 384 MiB LLC on c7a.48xlarge → ~4.6 GiB working set).
/// Override via `RIO_HW_BENCH_MEMBW_BYTES` so unit tests / VM tests
/// can run with 64 MiB and stay under CI memory.
fn membw_array_bytes() -> usize {
    std::env::var("RIO_HW_BENCH_MEMBW_BYTES")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(1536 * 1024 * 1024)
}

// ─── ioseq probe (O_DIRECT seq write) ────────────────────────────────

const IOSEQ_BLOCK: usize = 1024 * 1024;
/// `O_DIRECT` requires the userspace buffer to be block-device-
/// aligned. 4096 covers every admitted target (NVMe LBA + page size).
const IOSEQ_ALIGN: usize = 4096;
/// Loop the target file at this size so the bench stays under
/// `ephemeral-storage` even on the smallest size_class.
const IOSEQ_LOOP_BYTES: u64 = 256 * 1024 * 1024;
const IOSEQ_TARGET_SECS: f64 = 5.0;

/// 4096-aligned zeroed 1 MiB block for `O_DIRECT`. RAII so the
/// allocation is freed on every return path (including `?` from inside
/// `spawn_blocking`). The slice handed to `write_all` borrows this.
struct AlignedBlock {
    ptr: *mut u8,
    layout: Layout,
}

impl AlignedBlock {
    fn new() -> Self {
        let layout = Layout::from_size_align(IOSEQ_BLOCK, IOSEQ_ALIGN)
            .expect("1 MiB / 4096 align is always valid");
        // SAFETY: layout is non-zero-size; alloc_zeroed returns either
        // a valid aligned pointer or null. Null is unrecoverable (OOM
        // at builder init) — abort matches `Vec`'s behaviour.
        let ptr = unsafe { std::alloc::alloc_zeroed(layout) };
        assert!(!ptr.is_null(), "alloc_zeroed(1 MiB, 4096) returned null");
        Self { ptr, layout }
    }
    fn as_slice(&self) -> &[u8] {
        // SAFETY: ptr is non-null, 4096-aligned, points to IOSEQ_BLOCK
        // zeroed bytes valid for the lifetime of `self` (Drop frees).
        unsafe { std::slice::from_raw_parts(self.ptr, IOSEQ_BLOCK) }
    }
}

impl Drop for AlignedBlock {
    fn drop(&mut self) {
        // SAFETY: ptr/layout are exactly what `alloc_zeroed` returned.
        unsafe { std::alloc::dealloc(self.ptr, self.layout) }
    }
}

// The block is moved into `spawn_blocking` and only ever touched on
// that one thread; the raw pointer is what blocks the auto-derive.
unsafe impl Send for AlignedBlock {}

/// `ioseq` probe: `O_DIRECT|O_DSYNC` 1 MiB sequential writes to
/// `dir/.rio-ioseq-bench`, looped at `loop_bytes` (256 MiB in prod)
/// for `IOSEQ_TARGET_SECS` (~5 s). Returns sustained MB/s.
///
/// `O_DIRECT` defeats the page cache (otherwise this measures memcpy);
/// `O_DSYNC` forces each 1 MiB write to the device before returning so
/// the timer covers the device, not the request queue. `dir` is
/// `cfg.overlay_base_dir` — the overlays emptyDir, which is the same
/// volume builds write to (gp3 root or NVMe RAID0 per nodeClassRef).
///
/// Runs in `spawn_blocking`: the write loop is sync I/O by design.
/// Target file is removed on return (best-effort).
pub async fn ioseq_odirect_mbps(dir: &Path, loop_bytes: u64) -> std::io::Result<f64> {
    let path = dir.join(".rio-ioseq-bench");
    let path_for_cleanup = path.clone();
    let buf = AlignedBlock::new();
    let result = tokio::task::spawn_blocking(move || -> std::io::Result<f64> {
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .custom_flags((nix::fcntl::OFlag::O_DIRECT | nix::fcntl::OFlag::O_DSYNC).bits())
            .open(&path)?;
        let block = buf.as_slice();
        let start = Instant::now();
        // Track total bytes for the throughput numerator AND bytes
        // since last rewind for the loop-at-256MiB cap (two counters:
        // truncating `total` at rewind would make a fast NVMe report
        // ~50 MB/s regardless of true throughput).
        let mut total = 0u64;
        let mut since_rewind = 0u64;
        while start.elapsed().as_secs_f64() < IOSEQ_TARGET_SECS {
            f.write_all(block)?;
            total += IOSEQ_BLOCK as u64;
            since_rewind += IOSEQ_BLOCK as u64;
            if since_rewind >= loop_bytes {
                f.seek(SeekFrom::Start(0))?;
                since_rewind = 0;
            }
        }
        let elapsed = start.elapsed().as_secs_f64();
        Ok(total as f64 / elapsed / 1e6)
    })
    .await
    .expect("ioseq spawn_blocking join");
    let _ = std::fs::remove_file(&path_for_cleanup);
    result
}

// ─── orchestration ───────────────────────────────────────────────────

/// Spawn the K=3 microbench. Returns `None` if `hw_class` is empty
/// (resolver expired / non-k8s) — without an hw_class the row is
/// useless.
///
/// `bench_needed` is the `rio.build/hw-bench-needed` annotation
/// (downward-API → `RIO_HW_BENCH_NEEDED`, set by the controller when
/// any h in the intent's admissible set has <3 distinct pod_id AND
/// `requests.memory ≥ sla.hwBenchMemFloor`). When `false` only the
/// scalar `alu` probe runs and `membw`/`ioseq` are reported as `1.0`
/// (the reference-class value), so with default-uniform α the model
/// degrades to scalar — fail-closed.
///
/// `overlay_dir` is `cfg.overlay_base_dir` (the overlays emptyDir);
/// the ioseq probe writes there so it measures the same volume builds
/// write to.
///
/// Called from the resolve→bench task spawned at init
/// (`runtime/setup.rs`), which itself runs concurrently with FUSE
/// mount + cold-start (~30 s); the `[f64; K]` result is consumed once
/// the first `WorkAssignment` (and its assignment token) is in hand.
// r[impl sched.sla.hw-class.k3-bench]
pub fn spawn_measure(
    hw_class: &str,
    bench_needed: bool,
    overlay_dir: PathBuf,
) -> Option<tokio::task::JoinHandle<[f64; K]>> {
    if hw_class.is_empty() {
        tracing::debug!("hw_bench: no hw_class (downward volume empty); skipping");
        return None;
    }
    Some(tokio::spawn(async move {
        // alu + ioseq concurrent. alu is CPU-bound → spawn_blocking;
        // ioseq is its own spawn_blocking inside. Both ~5 s.
        let alu_h = tokio::task::spawn_blocking(alu_factor);
        let ioseq = if bench_needed {
            match ioseq_odirect_mbps(&overlay_dir, IOSEQ_LOOP_BYTES).await {
                Ok(mbps) => mbps / REF_IOSEQ_MBPS,
                Err(e) => {
                    // tmpfs (some CI / VM-test fixtures) rejects
                    // O_DIRECT with EINVAL. Fall through to 1.0 so
                    // the alu/membw dimensions still land.
                    tracing::warn!(error = %e, dir = %overlay_dir.display(),
                                   "hw_bench: ioseq O_DIRECT failed; reporting 1.0");
                    1.0
                }
            }
        } else {
            1.0
        };
        let alu = alu_h.await.expect("alu spawn_blocking join");
        // membw runs ALONE after alu+ioseq: STREAM triad saturates the
        // memory bus and would contaminate the alu pointer-chase.
        let membw = if bench_needed {
            let bytes = membw_array_bytes();
            tokio::task::spawn_blocking(move || stream_triad_gbps(bytes))
                .await
                .expect("membw spawn_blocking join")
                / REF_MEMBW_GBPS
        } else {
            1.0
        };
        [alu, membw, ioseq]
    }))
}

/// Best-effort: append the bench sample. Logs and returns on any
/// failure — a missed sample degrades normalization (hw_class stays at
/// `factor=1` until ≥3 distinct pods report), not the build itself.
///
/// `factor` is the already-awaited result of [`spawn_measure`]; the
/// caller (`spawn_build_task`) awaits the spawned resolve→bench task
/// and passes the `[f64; K]` here.
///
/// `submitting_tenant` is the pod's `RIO_TENANT` (downward-API): which
/// tenant's build this pod was spawned for. Feeds the per-tenant
/// median-of-medians defense (ADR-023 threat-model gap b).
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
    factor: [f64; K],
    submitting_tenant: &str,
    assignment_token: &str,
) {
    let factor_json = serde_json::json!({
        "alu": factor[ALU],
        "membw": factor[MEMBW],
        "ioseq": factor[IOSEQ],
    })
    .to_string();
    tracing::info!(%hw_class, %pod_id, %factor_json, %submitting_tenant,
                   "hw_bench: measured");
    let mut req = tonic::Request::new(rio_proto::types::AppendHwPerfSampleRequest {
        hw_class: hw_class.to_owned(),
        pod_id: pod_id.to_owned(),
        factor_json,
        submitting_tenant: submitting_tenant.to_owned(),
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
    use super::*;

    /// The alu bench must produce a finite positive factor. We can't
    /// assert a specific value (host-dependent), but `Instant::elapsed`
    /// is monotonic-nonzero so `REF_ALU_SECS / elapsed` is finite-
    /// positive on any real host. This catches the optimizer-elision
    /// regression (loop folds → elapsed ≈ 0 → factor = ∞).
    #[test]
    fn alu_produces_finite_positive_factor() {
        let f = alu_factor();
        assert!(f.is_finite(), "factor={f} (loop elided?)");
        assert!(f > 0.0, "factor={f}");
    }

    /// `alu_factor()` is deterministic-instruction-count → two
    /// back-to-back calls on the same host should agree within ±20%.
    /// Also bounds the factor to `(0.1, 100.0)`: anything outside means
    /// either the loop was elided (factor → ∞) or `ITERS` drifted so
    /// far from `REF_ALU_SECS` calibration that the hw-normalize math
    /// is meaningless.
    ///
    /// `threads-required = num-cpus` in `.config/nextest.toml` so this
    /// gets the machine to itself — the band was ±5% but flaked under
    /// parallel nextest load (contended L2 + DVFS); ±20% is the
    /// catches-elision-and-miscalibration band, not the production
    /// reproducibility band (the real bench runs on an idle pod at
    /// init, before any build).
    // r[verify sched.sla.hw-bench-append-only]
    #[test]
    fn alu_is_reproducible_within_band() {
        let f1 = alu_factor();
        let f2 = alu_factor();
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
        let ring = build_ring();
        let mut idx = 0u32;
        for _ in 0..RING_LEN {
            idx = ring[idx as usize];
        }
        assert_eq!(idx, 0, "RING_LEN hops returns to start");
        // Halfway is NOT back at start (rules out a 2-cycle).
        let mut idx = 0u32;
        for _ in 0..RING_LEN / 2 {
            idx = ring[idx as usize];
        }
        assert_ne!(idx, 0);
    }

    /// 64 MiB arrays — small enough for CI, large enough to spill L2 on
    /// every CI host. Band is wide (host-dependent); the assertion is
    /// "not zero, not infinite" — catches `black_box` regressions and
    /// unit errors (bytes vs. GB).
    #[test]
    fn stream_triad_returns_positive_gbps() {
        let bw = stream_triad_gbps(64 * 1024 * 1024);
        assert!(bw > 0.5 && bw < 1000.0, "implausible membw: {bw} GB/s");
    }

    /// O_DIRECT mechanics: aligned buffer accepted, file
    /// created+removed, finite-positive MB/s. NOT a throughput band
    /// — the nix sandbox's build dir may sit on overlayfs/zfs where
    /// `O_DIRECT` is accepted but silently un-honored (page-cache
    /// speed, ~60 GB/s observed), and tmpfs rejects it outright with
    /// `EINVAL`. Both are fine for a unit test of the code path; the
    /// production probe targets `/var/rio/overlays` which is always
    /// ext4-on-EBS or ext4-on-NVMe.
    #[tokio::test]
    async fn ioseq_returns_positive_mbps() {
        let dir = tempfile::tempdir().unwrap();
        match ioseq_odirect_mbps(dir.path(), 64 * 1024 * 1024).await {
            Ok(mbps) => {
                assert!(
                    mbps.is_finite() && mbps > 1.0,
                    "implausible ioseq: {mbps} MB/s"
                );
                assert!(
                    !dir.path().join(".rio-ioseq-bench").exists(),
                    "bench file not cleaned up"
                );
            }
            Err(e) if e.raw_os_error() == Some(nix::libc::EINVAL) => {
                eprintln!("ioseq: O_DIRECT unsupported on tempdir fs ({e}); skipped");
            }
            Err(e) => panic!("ioseq failed: {e}"),
        }
    }

    /// Full orchestration: K=3 vector, all finite-positive, alu/ioseq
    /// concurrent then membw alone. `bench_needed=true` exercises all
    /// three probes; `RIO_HW_BENCH_MEMBW_BYTES` keeps the working set
    /// CI-sized.
    // r[verify sched.sla.hw-class.k3-bench]
    #[tokio::test(flavor = "multi_thread")]
    async fn spawn_measure_returns_k3_vector() {
        // Safe: nextest runs each test in its own process.
        unsafe { std::env::set_var("RIO_HW_BENCH_MEMBW_BYTES", "67108864") };
        let dir = tempfile::tempdir().unwrap();
        let h = spawn_measure("test-hw", true, dir.path().to_path_buf()).unwrap();
        let v = h.await.unwrap();
        assert_eq!(v.len(), K);
        for (d, &f) in v.iter().enumerate() {
            assert!(f.is_finite() && f > 0.0, "factor[{d}]={f}");
        }
    }

    /// Fail-closed: `bench_needed=false` → only alu runs; membw/ioseq
    /// report exactly 1.0 (reference-class value).
    // r[verify sched.sla.hw-class.k3-bench]
    #[tokio::test(flavor = "multi_thread")]
    async fn spawn_measure_scalar_only_when_not_needed() {
        let h = spawn_measure("test-hw", false, std::env::temp_dir()).unwrap();
        let v = h.await.unwrap();
        assert!(v[ALU].is_finite() && v[ALU] > 0.0);
        assert_eq!(v[MEMBW], 1.0);
        assert_eq!(v[IOSEQ], 1.0);
    }

    /// Empty hw_class → `None` (no row written).
    #[test]
    fn spawn_measure_none_on_empty_hw_class() {
        assert!(spawn_measure("", true, std::env::temp_dir()).is_none());
    }
}
