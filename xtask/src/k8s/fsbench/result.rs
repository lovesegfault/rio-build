//! result.json (schema `fsbench/v1`) — the comparable artifact.
//!
//! Assembled from three sources: the parsed PERF stream (the
//! measurement source of truth), the co-tenancy report (placement +
//! cluster-side metric deltas), and local context (git, provider).
//! A baseline file is byte-identical in shape to a result file.

use std::collections::BTreeMap;
use std::path::Path;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use super::cotenancy::CotenancyReport;
use super::parse::{ParsedRun, PerfLine};

pub const SCHEMA: &str = "fsbench/v1";

/// Fraction of the manifest's UNIQUE-chunk bytes (storm subset for the
/// cold window, whole dataset for the whole-run fallback) that must
/// show up as mountd Promote traffic for the run to count as honestly
/// cold. Unique bytes, not logical: real closure content dedupes.
pub const COLD_HONESTY_PROMOTE_FRACTION: f64 = 0.95;
/// Ceiling on remote re-fetch during the warm window (fraction of
/// dataset bytes) — above this, "warm" reads were quietly remote.
pub const WARM_HONESTY_REMOTE_FRACTION: f64 = 0.01;

#[derive(Serialize, Deserialize, Clone)]
pub struct ResultV1 {
    pub schema: String,
    pub run_id: String,
    pub seed: String,
    pub created_at: String,
    pub git: GitInfo,
    pub cluster: ClusterInfo,
    pub placement: Placement,
    pub workload: Workload,
    pub phases: BTreeMap<String, PhaseMetrics>,
    pub cluster_metrics: ClusterMetrics,
    /// Populated by the auto-compare after the run (absent in baseline
    /// files saved from this result).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub compare: Option<CompareBlock>,
}

#[derive(Serialize, Deserialize, Clone)]
pub struct GitInfo {
    pub commit: String,
    pub dirty: bool,
    pub image_tag: String,
}

#[derive(Serialize, Deserialize, Clone)]
pub struct ClusterInfo {
    pub provider: String,
    pub context: String,
}

#[derive(Serialize, Deserialize, Clone)]
pub struct Placement {
    pub node: Option<String>,
    pub instance_type: Option<String>,
    pub capacity_type: Option<String>,
    /// Kernel as reported by the bench process inside the sandbox
    /// (`uname -r` ≙ the node kernel).
    pub kernel: String,
    pub ami: Option<String>,
    /// `exact` = the bench build was matched to its executor pod and
    /// node; `unattributed` blocks baseline save and compare.
    pub attribution: String,
    pub contended: bool,
    pub max_co_tenants: u64,
    pub cotenancy_samples: u64,
}

#[derive(Serialize, Deserialize, Clone)]
pub struct Workload {
    pub version: u32,
    /// blake3 over the dataset's tree listing (paths + per-file
    /// digests + symlink targets + exec bits) — the dataset half of
    /// the identity key: a harvest or layout change refuses compares.
    pub dataset_digest: String,
    pub files: u64,
    pub total_bytes: u64,
    /// Deduplicated FastCDC chunk bytes (the store's own chunker) —
    /// the honesty references; recorded so the artifact explains its
    /// own gate arithmetic.
    pub unique_chunk_bytes: u64,
    pub unique_chunk_bytes_storm: u64,
    /// jq source + toolchain identity for the jq_build compile phases
    /// (store-path basenames; absent when the phases were skipped). A
    /// bump to either refuses compares.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub jq_src: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub toolchain: Option<String>,
    pub closure: String,
    pub phases: Vec<String>,
    pub reps: u32,
}

#[derive(Serialize, Deserialize, Clone)]
pub struct PhaseMetrics {
    pub start_epoch_ms: u64,
    pub end_epoch_ms: u64,
    pub reps: u32,
    pub metrics: BTreeMap<String, Metric>,
}

/// One quoted metric. Scalar metrics carry `value`; latency metrics
/// carry the percentile block (fields absent below their sample-count
/// floors). Warm metrics carry `rep_spread` = (max−min)/median across
/// reps — the in-run noise estimate compare verdicts key off.
#[derive(Serialize, Deserialize, Clone, Default)]
pub struct Metric {
    pub unit: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub n: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub value: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub p50: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub p99: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub p999: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rep_spread: Option<f64>,
}

#[derive(Serialize, Deserialize, Clone)]
pub struct ClusterMetrics {
    /// false when attribution failed or the executor pod restarted
    /// mid-run (counters reset → deltas are garbage).
    pub valid: bool,
    /// The cold-honesty verdict: Promote traffic accounts for the cold
    /// phase's bytes (windowed delta, or the whole-run delta as the
    /// late-attribution fallback — see [`cluster_metrics`]) AND
    /// warm-window remote fetch stayed ≤1% of the dataset. `None` when
    /// it could not be computed (unattributed / invalid / scrape
    /// failed). A `Some(false)` run is "dishonest-cold": its numbers
    /// are reported but a baseline save is refused — and so is a
    /// `None` run (unverifiable is not verified).
    pub honest_cold: Option<bool>,
    /// Present when honesty was established via the whole-run fallback
    /// rather than the cold-window delta — i.e. the windowed check
    /// undercounted because sampling latched after the cold phase
    /// began.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub honesty_note: Option<String>,
    pub mountd: Option<MountdDeltas>,
    pub builder: Option<BuilderDeltas>,
}

#[derive(Serialize, Deserialize, Clone, Default)]
pub struct MountdDeltas {
    pub promote_bytes_total_delta: f64,
    /// Promote-bytes delta over the read_storm_cold window only — the
    /// numerator of the cold-honesty check.
    pub promote_bytes_cold_window_delta: Option<f64>,
    pub request_count_delta: BTreeMap<String, f64>,
    pub request_sum_delta_s: BTreeMap<String, f64>,
}

#[derive(Serialize, Deserialize, Clone, Default)]
pub struct BuilderDeltas {
    pub open_case_total_delta: BTreeMap<String, f64>,
    pub fetch_bytes_total_delta: BTreeMap<String, f64>,
    pub fetch_bytes_remote_warm_window_delta: Option<f64>,
    pub open_seconds_count_delta: f64,
}

#[derive(Serialize, Deserialize, Clone)]
pub struct CompareBlock {
    pub baseline: String,
    pub verdicts: Vec<String>,
}

/// Median of already-collected per-rep values.
fn median(mut xs: Vec<f64>) -> f64 {
    xs.sort_by(|a, b| a.total_cmp(b));
    xs[xs.len() / 2]
}

/// (max−min)/median across reps; `None` for single-rep phases.
fn rep_spread(xs: &[f64]) -> Option<f64> {
    if xs.len() < 2 {
        return None;
    }
    let (mut min, mut max) = (f64::INFINITY, f64::NEG_INFINITY);
    for &x in xs {
        min = min.min(x);
        max = max.max(x);
    }
    let med = median(xs.to_vec());
    (med != 0.0).then(|| (max - min) / med)
}

/// Aggregate one metric key across the per-rep PERF lines of a phase:
/// quoted value = median across reps, noise = rep_spread.
fn agg(lines: &[&PerfLine], key: &str, unit: &str, n: Option<u64>) -> Option<Metric> {
    let vals: Vec<f64> = lines.iter().filter_map(|l| l.f64(key)).collect();
    if vals.is_empty() {
        return None;
    }
    Some(Metric {
        unit: unit.into(),
        n,
        value: Some(median(vals.clone())),
        rep_spread: rep_spread(&vals),
        ..Default::default()
    })
}

/// Latency percentile block from per-rep lines: each percentile is the
/// median across reps of that rep's percentile; rep_spread is quoted
/// on p50 (the most stable tail).
fn agg_latency(lines: &[&PerfLine], prefix: &str, n: Option<u64>) -> Option<Metric> {
    let pick = |suffix: &str| -> Option<f64> {
        let vals: Vec<f64> = lines
            .iter()
            .filter_map(|l| l.f64(&format!("{prefix}_{suffix}")))
            .collect();
        (!vals.is_empty()).then(|| median(vals.clone()))
    };
    let p50 = pick("p50")?;
    let p50s: Vec<f64> = lines
        .iter()
        .filter_map(|l| l.f64(&format!("{prefix}_p50")))
        .collect();
    Some(Metric {
        unit: "ns".into(),
        n,
        p50: Some(p50),
        p99: pick("p99"),
        p999: pick("p999"),
        max: pick("max"),
        rep_spread: rep_spread(&p50s),
        ..Default::default()
    })
}

/// Build the phase map from the parsed PERF stream. Phase keys are
/// flat (`open_storm_pass2`, `randread_warm`) so metric paths used by
/// compare stay unambiguous (`randread_warm.iops`).
pub fn phases_from_run(run: &ParsedRun) -> BTreeMap<String, PhaseMetrics> {
    let mut out = BTreeMap::new();

    let cold = run.perf_named("read_storm_cold");
    if !cold.is_empty() {
        let mut m = storm_metrics(&cold);
        if let Some(x) = agg(&cold, "checksum_ok", "count", None) {
            m.insert("checksum_ok".into(), x);
        }
        if let Some(x) = agg(&cold, "bytes", "count", None) {
            m.insert("bytes".into(), x);
        }
        insert_phase(
            &mut out,
            run,
            "read_storm_cold",
            ("read_storm_cold", 1),
            1,
            m,
        );
    }
    let warm = run.perf_named("read_storm_warm");
    if !warm.is_empty() {
        let m = storm_metrics(&warm);
        insert_phase(
            &mut out,
            run,
            "read_storm_warm",
            ("read_storm_warm", 1),
            warm.len() as u32,
            m,
        );
    }

    for pass in [1u32, 2] {
        let lines: Vec<&PerfLine> = run
            .perf_named("open_storm")
            .into_iter()
            .filter(|l| l.f64("pass") == Some(f64::from(pass)))
            .collect();
        if lines.is_empty() {
            continue;
        }
        let n = lines.first().and_then(|l| l.f64("files")).map(|v| v as u64);
        let mut m = BTreeMap::new();
        if let Some(x) = agg_latency(&lines, "open_ns", n) {
            m.insert("open_ns".into(), x);
        }
        if let Some(x) = agg(&lines, "fstat_ns_p50", "ns", n) {
            m.insert("fstat_ns_p50".into(), x);
        }
        let key = format!("open_storm_pass{pass}");
        insert_phase(&mut out, run, &key, ("open_storm", pass), 1, m);
    }

    insert_randread(
        &mut out,
        run,
        "randread_cold",
        ("randread_cold", 1),
        "castore",
        "cold",
    );
    insert_randread(
        &mut out,
        run,
        "randread_warm",
        ("randread_warm", 1),
        "castore",
        "warm",
    );
    insert_randread(
        &mut out,
        run,
        "randread_local_warm",
        ("randread_local_warm", 1),
        "local",
        "warm",
    );

    // jq_build compile phases: one PERF line per state (cold rep 1,
    // warm rep 2), wall-clock value metrics.
    for (key, state, window_rep) in [
        ("jq_build_cold", "cold", 1u32),
        ("jq_build_warm", "warm", 2),
    ] {
        let lines: Vec<&PerfLine> = run
            .perf_named("jq_build")
            .into_iter()
            .filter(|l| l.str("state") == Some(state))
            .collect();
        if lines.is_empty() {
            continue;
        }
        let mut m = BTreeMap::new();
        for metric in ["configure_wall_ms", "make_wall_ms", "total_wall_ms"] {
            if let Some(x) = agg(&lines, metric, "ms", None) {
                m.insert(metric.into(), x);
            }
        }
        insert_phase(&mut out, run, key, (key, window_rep), lines.len() as u32, m);
    }

    let copy = run.perf_named("copy_to_local");
    if !copy.is_empty() {
        let mut m = BTreeMap::new();
        if let Some(x) = agg(&copy, "mib_s", "mib_s", None) {
            m.insert("mib_s".into(), x);
        }
        insert_phase(&mut out, run, "copy_to_local", ("copy_to_local", 1), 1, m);
    }
    let local = run.perf_named("read_storm_local");
    if !local.is_empty() {
        let m = storm_metrics(&local);
        insert_phase(
            &mut out,
            run,
            "read_storm_local",
            ("read_storm_local", 1),
            1,
            m,
        );
    }
    let local_warm = run.perf_named("read_storm_local_warm");
    if !local_warm.is_empty() {
        let m = storm_metrics(&local_warm);
        insert_phase(
            &mut out,
            run,
            "read_storm_local_warm",
            ("read_storm_local_warm", 1),
            local_warm.len() as u32,
            m,
        );
    }

    // Derived slowdown ratios (castore time over local time, i.e.
    // local throughput over castore throughput — >1 means the mount is
    // slower than node disk). A drifting LOCAL baseline flags
    // hardware/kernel drift, which compare reports as baseline-drift
    // rather than a FUSE regression.
    let mut ratios = BTreeMap::new();
    if let Some(m) = slowdown_ratio(&out, "read_storm_local_warm", "read_storm_warm", "mib_s") {
        ratios.insert("warm_read_slowdown".to_string(), m);
    }
    if let Some(m) = slowdown_ratio(&out, "randread_local_warm", "randread_warm", "iops") {
        ratios.insert("randread_warm_slowdown".to_string(), m);
    }
    if !ratios.is_empty() {
        out.insert(
            "ratios".into(),
            PhaseMetrics {
                start_epoch_ms: 0,
                end_epoch_ms: 0,
                reps: 1,
                metrics: ratios,
            },
        );
    }
    out
}

fn insert_phase(
    out: &mut BTreeMap<String, PhaseMetrics>,
    run: &ParsedRun,
    key: &str,
    window: (&str, u32),
    reps: u32,
    metrics: BTreeMap<String, Metric>,
) {
    let w = run.window(window.0, window.1);
    out.insert(
        key.to_string(),
        PhaseMetrics {
            start_epoch_ms: w.map_or(0, |w| w.start_epoch_ms),
            end_epoch_ms: w.map_or(0, |w| w.end_epoch_ms),
            reps,
            metrics,
        },
    );
}

/// mib_s + open/read latency blocks shared by every read-storm flavor.
fn storm_metrics(lines: &[&PerfLine]) -> BTreeMap<String, Metric> {
    let n = lines.first().and_then(|l| l.f64("files")).map(|v| v as u64);
    let mut m = BTreeMap::new();
    if let Some(x) = agg(lines, "mib_s", "mib_s", None) {
        m.insert("mib_s".into(), x);
    }
    if let Some(x) = agg_latency(lines, "open_ns", n) {
        m.insert("open_ns".into(), x);
    }
    if let Some(x) = agg_latency(lines, "read_ns", n) {
        m.insert("read_ns".into(), x);
    }
    m
}

fn insert_randread(
    out: &mut BTreeMap<String, PhaseMetrics>,
    run: &ParsedRun,
    key: &str,
    window: (&str, u32),
    target: &str,
    state: &str,
) {
    let lines: Vec<&PerfLine> = run
        .perf
        .iter()
        .filter(|l| {
            l.name == "randread" && l.str("target") == Some(target) && l.str("state") == Some(state)
        })
        .collect();
    if lines.is_empty() {
        return;
    }
    let n = lines.first().and_then(|l| l.f64("ios")).map(|v| v as u64);
    let mut m = BTreeMap::new();
    if let Some(x) = agg_latency(&lines, "io_ns", n) {
        m.insert("io_ns".into(), x);
    }
    if let Some(x) = agg(&lines, "iops", "iops", None) {
        m.insert("iops".into(), x);
    }
    if let Some(x) = agg(&lines, "mib_s", "mib_s", None) {
        m.insert("mib_s".into(), x);
    }
    if let Some(x) = agg(&lines, "direct", "count", None) {
        m.insert("direct".into(), x);
    }
    insert_phase(out, run, key, window, lines.len() as u32, m);
}

/// `local_phase.value / castore_phase.value` for `key` — slowdown of
/// the mount relative to node disk.
fn slowdown_ratio(
    out: &BTreeMap<String, PhaseMetrics>,
    local: &str,
    castore: &str,
    key: &str,
) -> Option<Metric> {
    let num = out.get(local)?.metrics.get(key)?.value?;
    let den = out.get(castore)?.metrics.get(key)?.value?;
    (den != 0.0).then(|| Metric {
        unit: "ratio".into(),
        value: Some(num / den),
        ..Default::default()
    })
}

/// Cluster-metric deltas + the honesty verdict, computed from the
/// timestamped co-tenancy samples against the parsed phase windows.
pub fn cluster_metrics(run: &ParsedRun, report: &CotenancyReport) -> ClusterMetrics {
    if report.attribution != "exact" || report.samples.len() < 2 {
        return ClusterMetrics {
            valid: false,
            honest_cold: None,
            honesty_note: None,
            mountd: None,
            builder: None,
        };
    }
    let valid = !report.executor_uid_changed;
    let first = &report.samples[0];
    let last = &report.samples[report.samples.len() - 1];

    let delta_map = |a: &BTreeMap<String, f64>, b: &BTreeMap<String, f64>| {
        b.iter()
            .map(|(k, vb)| (k.clone(), vb - a.get(k).copied().unwrap_or(0.0)))
            .collect::<BTreeMap<String, f64>>()
    };

    // Window delta: counter at the last sample ≤ window start vs the
    // first sample ≥ window end. 5s sampling means ±5s slop — the
    // honesty thresholds (0.95×, 1%) leave room for it.
    let window_delta = |window: Option<(u64, u64)>,
                        get: &dyn Fn(&super::cotenancy::Sample) -> Option<f64>|
     -> Option<f64> {
        let (start, end) = window?;
        let before = report
            .samples
            .iter()
            .rev()
            .find(|s| s.epoch_ms <= start)
            .or(report.samples.first())?;
        let after = report.samples.iter().find(|s| s.epoch_ms >= end)?;
        Some(get(after)? - get(before)?)
    };

    let cold_w = run
        .window("read_storm_cold", 1)
        .map(|w| (w.start_epoch_ms, w.end_epoch_ms));
    // Warm window: first read_storm_warm rep start → last rep end.
    let warm_w = {
        let reps: Vec<_> = run
            .phases
            .iter()
            .filter(|w| w.name == "read_storm_warm")
            .collect();
        match (reps.first(), reps.last()) {
            (Some(a), Some(b)) => Some((a.start_epoch_ms, b.end_epoch_ms)),
            _ => None,
        }
    };

    let mountd = MountdDeltas {
        promote_bytes_total_delta: last.mountd_promote_bytes - first.mountd_promote_bytes,
        promote_bytes_cold_window_delta: window_delta(cold_w, &|s| Some(s.mountd_promote_bytes)),
        request_count_delta: delta_map(&first.mountd_request_count, &last.mountd_request_count),
        request_sum_delta_s: delta_map(&first.mountd_request_sum, &last.mountd_request_sum),
    };
    let builder = match (&first.builder, &last.builder) {
        (Some(fb), Some(lb)) => Some(BuilderDeltas {
            open_case_total_delta: delta_map(&fb.open_case, &lb.open_case),
            fetch_bytes_total_delta: delta_map(&fb.fetch_bytes, &lb.fetch_bytes),
            fetch_bytes_remote_warm_window_delta: window_delta(warm_w, &|s| {
                s.builder
                    .as_ref()
                    .and_then(|b| b.fetch_bytes.get("remote").copied())
            }),
            open_seconds_count_delta: lb.open_seconds_count - fb.open_seconds_count,
        }),
        _ => None,
    };

    // Honesty: Promote traffic accounts for the cold bytes; warm
    // window stayed local. The references are the manifest's
    // UNIQUE-CHUNK byte counts (stamped into the PERF meta line), not
    // logical bytes: the dataset is real closure content with
    // natural cross-file duplication, and dedupe makes logical-bytes
    // arithmetic convict honest runs (a deduped read promotes once).
    // The cold window references the storm subset's unique bytes; the
    // whole-run fallback references the whole dataset's (the randread
    // fill promotes the reserve too).
    //
    // Why a fallback at all: attribution latches only after the bench
    // drv shows up in `workers --json` (a heartbeat plus up-to-5s tick
    // of lag) and read_storm_cold is the FIRST phase — Promote bytes
    // landing before the first sample vanish from the windowed delta,
    // which would convict an honest run. Genuine dishonesty = BOTH
    // deltas under their bounds.
    let mut honesty_note = None;
    let honest_cold = (|| {
        if !valid {
            return None;
        }
        let unique_storm: f64 = run.meta.get("unique_chunk_bytes_storm")?.parse().ok()?;
        let unique_total: f64 = run.meta.get("unique_chunk_bytes")?.parse().ok()?;
        let dataset_bytes: f64 = run.meta.get("dataset_bytes")?.parse().ok()?;
        let warm_remote = builder
            .as_ref()
            .and_then(|b| b.fetch_bytes_remote_warm_window_delta)?;
        let warm_ok = warm_remote <= WARM_HONESTY_REMOTE_FRACTION * dataset_bytes;

        let bound = COLD_HONESTY_PROMOTE_FRACTION * unique_storm;
        let window_ok = mountd.promote_bytes_cold_window_delta.map(|d| d >= bound);
        let run_ok =
            mountd.promote_bytes_total_delta >= COLD_HONESTY_PROMOTE_FRACTION * unique_total;
        let cold_ok = match window_ok {
            Some(true) => true,
            Some(false) if run_ok => {
                honesty_note = Some(
                    "late-attribution undercount: cold-window Promote delta is below the \
                     bound, but the whole-run delta covers it (sampling latched after the \
                     cold phase began)"
                        .into(),
                );
                true
            }
            None if run_ok => {
                honesty_note = Some(
                    "cold window not bracketed by samples; honesty established from the \
                     whole-run Promote delta"
                        .into(),
                );
                true
            }
            _ => false,
        };
        Some(cold_ok && warm_ok)
    })();

    ClusterMetrics {
        valid,
        honest_cold,
        honesty_note,
        mountd: Some(mountd),
        // None when the executor scrape never succeeded — fabricated
        // zero deltas would read as "no FUSE activity at all".
        builder,
    }
}

pub fn write(result: &ResultV1, path: &Path) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(path, serde_json::to_vec_pretty(result)?)
        .with_context(|| format!("write {}", path.display()))
}

pub fn read(path: &Path) -> Result<ResultV1> {
    let body = std::fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?;
    serde_json::from_str(&body).with_context(|| format!("parse {}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::k8s::fsbench::parse::parse_log;

    fn three_warm_reps() -> ParsedRun {
        parse_log(
            "x> PERF meta seed=s dataset_bytes=1000 files=3 kernel=k fsbench_rev=1\n\
             x> PHASE read_storm_warm start epoch_ms=10 rep=1\n\
             x> PHASE read_storm_warm end epoch_ms=20 rep=1\n\
             x> PERF read_storm_warm rep=1 files=3 bytes=900 wall_ms=10 mib_s=100.0 open_ns_p50=10 open_ns_p99=50 read_ns_p50=5 read_ns_p99=9\n\
             x> PERF read_storm_warm rep=2 files=3 bytes=900 wall_ms=10 mib_s=110.0 open_ns_p50=12 open_ns_p99=55 read_ns_p50=5 read_ns_p99=9\n\
             x> PERF read_storm_warm rep=3 files=3 bytes=900 wall_ms=10 mib_s=90.0 open_ns_p50=11 open_ns_p99=60 read_ns_p50=5 read_ns_p99=9\n",
        )
    }

    #[test]
    fn warm_phase_quotes_median_and_rep_spread() {
        let phases = phases_from_run(&three_warm_reps());
        let warm = &phases["read_storm_warm"];
        assert_eq!(warm.reps, 3);
        let mib = &warm.metrics["mib_s"];
        // Median of {100, 110, 90} = 100; spread = (110-90)/100 = 0.2.
        assert_eq!(mib.value, Some(100.0));
        assert!((mib.rep_spread.unwrap() - 0.2).abs() < 1e-9);
        // Latency: median across reps of per-rep p50 {10,12,11} = 11.
        assert_eq!(warm.metrics["open_ns"].p50, Some(11.0));
        // 3 files is far below the p99 floor — the binary omitted the
        // key, so the aggregate must NOT invent one… but the fixture
        // does carry p99 (floors are the binary's job; the parser
        // trusts what was emitted).
        assert_eq!(warm.metrics["open_ns"].p99, Some(55.0));
    }

    #[test]
    fn ratios_derive_from_local_twins() {
        let run = parse_log(
            "x> PERF read_storm_warm rep=1 files=1 bytes=1 wall_ms=1 mib_s=500.0 open_ns_p50=1 read_ns_p50=1\n\
             x> PERF read_storm_local_warm rep=1 files=1 bytes=1 wall_ms=1 mib_s=1000.0 open_ns_p50=1 read_ns_p50=1\n",
        );
        let phases = phases_from_run(&run);
        let r = &phases["ratios"].metrics["warm_read_slowdown"];
        // local 1000 / castore 500 → the mount is 2× slower.
        assert_eq!(r.value, Some(2.0));
    }

    #[test]
    fn jq_build_lines_split_into_per_state_phases() {
        let run = parse_log(
            "x> PHASE jq_build_cold start epoch_ms=10 rep=1\n\
             x> PHASE jq_build_cold end epoch_ms=30010 rep=1\n\
             x> PERF jq_build state=cold rep=1 configure_wall_ms=9000 make_wall_ms=21000 total_wall_ms=30000\n\
             x> PHASE jq_build_warm start epoch_ms=30011 rep=2\n\
             x> PHASE jq_build_warm end epoch_ms=48011 rep=2\n\
             x> PERF jq_build state=warm rep=2 configure_wall_ms=5000 make_wall_ms=13000 total_wall_ms=18000\n",
        );
        let phases = phases_from_run(&run);
        let cold = &phases["jq_build_cold"];
        assert_eq!(cold.metrics["total_wall_ms"].value, Some(30000.0));
        assert_eq!(cold.metrics["configure_wall_ms"].value, Some(9000.0));
        assert_eq!(
            (cold.start_epoch_ms, cold.end_epoch_ms),
            (10, 30010),
            "window paired under the per-state phase name"
        );
        let warm = &phases["jq_build_warm"];
        assert_eq!(warm.metrics["make_wall_ms"].value, Some(13000.0));
        assert_eq!(warm.start_epoch_ms, 30011);
    }

    #[test]
    fn result_round_trips_through_json() {
        let run = three_warm_reps();
        let result = ResultV1 {
            schema: SCHEMA.into(),
            run_id: "r".into(),
            seed: "s".into(),
            created_at: "2026-06-02T00:00:00Z".into(),
            git: GitInfo {
                commit: "abc".into(),
                dirty: false,
                image_tag: "abc".into(),
            },
            cluster: ClusterInfo {
                provider: "eks".into(),
                context: "rio-jorg".into(),
            },
            placement: Placement {
                node: Some("n1".into()),
                instance_type: Some("c7a.4xlarge".into()),
                capacity_type: Some("od".into()),
                kernel: "6.12.20".into(),
                ami: None,
                attribution: "exact".into(),
                contended: false,
                max_co_tenants: 1,
                cotenancy_samples: 37,
            },
            workload: Workload {
                version: 1,
                dataset_digest: "abc123".into(),
                files: 6556,
                total_bytes: 1_932_848_097,
                unique_chunk_bytes: 1_500_000_000,
                unique_chunk_bytes_storm: 1_100_000_000,
                jq_src: Some("hash-jq-1.7.1.tar.gz".into()),
                toolchain: Some("hash-gcc-wrapper-14".into()),
                closure: "python3".into(),
                phases: vec!["read_storm_warm".into()],
                reps: 3,
            },
            phases: phases_from_run(&run),
            cluster_metrics: ClusterMetrics {
                valid: true,
                honest_cold: Some(true),
                honesty_note: None,
                mountd: Some(MountdDeltas::default()),
                builder: Some(BuilderDeltas::default()),
            },
            compare: None,
        };
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("result.json");
        write(&result, &p).unwrap();
        let back = read(&p).unwrap();
        assert_eq!(back.schema, SCHEMA);
        assert_eq!(back.placement.attribution, "exact");
        assert_eq!(
            back.phases["read_storm_warm"].metrics["mib_s"].value,
            Some(100.0)
        );
        // compare:None must serialize as an absent key, not null — a
        // baseline file is shape-identical to a result file.
        let text = std::fs::read_to_string(&p).unwrap();
        assert!(!text.contains("\"compare\""));
    }

    // ── cold-honesty: windowed check + whole-run fallback ──────────
    //
    // The synthetic geometry below: cold window 1000..61000, storm
    // unique-chunk bytes 1000 (window bound = 950), whole-dataset
    // unique 1000 (whole-run bound = 950), logical dataset 2000
    // (warm-window ceiling = 20), warm window 61001..62001. Logical >
    // unique on purpose — the references must be the unique counts.

    use crate::k8s::fsbench::cotenancy::{BuilderSample, CotenancyReport, Sample};

    fn honesty_run() -> ParsedRun {
        parse_log(
            "x> PERF meta seed=s dataset_bytes=2000 unique_chunk_bytes=1000 unique_chunk_bytes_storm=1000 files=3 kernel=6.12.20 fsbench_rev=1 workload_version=1\n\
             x> PHASE read_storm_cold start epoch_ms=1000 rep=1\n\
             x> PHASE read_storm_cold end epoch_ms=61000 rep=1\n\
             x> PERF read_storm_cold rep=1 files=2 bytes=2000 wall_ms=60000 mib_s=10.0 open_ns_p50=1 read_ns_p50=1 checksum_ok=2\n\
             x> PHASE read_storm_warm start epoch_ms=61001 rep=1\n\
             x> PHASE read_storm_warm end epoch_ms=62001 rep=1\n\
             x> PERF read_storm_warm rep=1 files=2 bytes=2000 wall_ms=1000 mib_s=100.0 open_ns_p50=1 read_ns_p50=1\n",
        )
    }

    fn sample(epoch_ms: u64, promote: f64) -> Sample {
        Sample {
            epoch_ms,
            connections_current: 1.0,
            mountd_promote_bytes: promote,
            mountd_request_count: BTreeMap::new(),
            mountd_request_sum: BTreeMap::new(),
            builder: Some(BuilderSample {
                open_case: BTreeMap::new(),
                fetch_bytes: BTreeMap::from([("remote".to_string(), 0.0)]),
                open_seconds_count: 0.0,
            }),
        }
    }

    fn exact_report(samples: Vec<Sample>) -> CotenancyReport {
        CotenancyReport {
            attribution: "exact".into(),
            samples,
            ..Default::default()
        }
    }

    #[test]
    fn bracketed_cold_window_is_honest_without_note() {
        // Sampling started before the cold phase: the windowed delta
        // alone proves honesty; no fallback, no note. The promote
        // delta (1000) clears the UNIQUE-chunk bound (950) while
        // falling well short of 0.95× the LOGICAL bytes (1900) — a
        // logical-bytes reference would convict this honest run, which
        // is exactly the dedupe arithmetic the unique reference fixes.
        let report = exact_report(vec![
            sample(500, 0.0),
            sample(61_500, 1000.0),
            sample(70_000, 1000.0),
        ]);
        let cm = cluster_metrics(&honesty_run(), &report);
        assert_eq!(cm.honest_cold, Some(true));
        assert_eq!(cm.honesty_note, None);
    }

    #[test]
    fn late_attribution_undercount_falls_back_to_whole_run_delta() {
        // Attribution latched 30s into the 60s cold phase: the first
        // sample's counter already contains 600 promoted bytes, so the
        // windowed delta sees only 1100−600=500 < 950 — an honest run
        // that the windowed check alone would convict. The whole-run
        // delta (1600−600=1000 ≥ 950) rescues it, with the note
        // explaining why.
        let report = exact_report(vec![
            sample(30_000, 600.0),
            sample(61_500, 1100.0),
            sample(120_000, 1600.0),
        ]);
        let cm = cluster_metrics(&honesty_run(), &report);
        assert_eq!(cm.honest_cold, Some(true));
        assert!(
            cm.honesty_note
                .as_deref()
                .is_some_and(|n| n.contains("late-attribution")),
            "note must name the mechanism: {:?}",
            cm.honesty_note
        );
    }

    #[test]
    fn genuinely_dishonest_fails_both_deltas() {
        // Barely any Promote traffic in the window OR across the whole
        // run: the bytes did not come through Promote — dishonest, and
        // no note (there is nothing to excuse).
        let report = exact_report(vec![
            sample(500, 0.0),
            sample(61_500, 100.0),
            sample(120_000, 200.0),
        ]);
        let cm = cluster_metrics(&honesty_run(), &report);
        assert_eq!(cm.honest_cold, Some(false));
        assert_eq!(cm.honesty_note, None);
    }

    #[test]
    fn failed_executor_scrape_yields_builder_none_not_zeros() {
        // No executor scrape ever succeeded: builder deltas must be
        // absent, not Some(all-zero) — fabricated zeros would read as
        // "no FUSE activity", and honesty (which needs the warm-window
        // fetch delta) must stay None, not pass vacuously.
        let mut samples = vec![sample(500, 0.0), sample(70_000, 2000.0)];
        for s in &mut samples {
            s.builder = None;
        }
        let cm = cluster_metrics(&honesty_run(), &exact_report(samples));
        assert!(cm.builder.is_none());
        assert_eq!(cm.honest_cold, None);
    }
}
