//! The compare engine: identity-key gate, thresholded per-metric
//! verdicts, exit-code contract.
//!
//! Refusal beats a wrong answer: results from different hardware,
//! kernels, workloads, or concurrency states are not comparable, and
//! pretending otherwise produces confident nonsense. `--force`
//! overrides the gate but marks every verdict `untrusted`.

use std::fmt;

use super::result::ResultV1;

/// Verdicts that exit nonzero: ≥1 `regressed` → 3; identity refusal
/// → 2 (operator scripting only — fsbench is not CI-gated).
pub const EXIT_REFUSED: i32 = 2;
pub const EXIT_REGRESSED: i32 = 3;

#[derive(Debug, Clone, Copy)]
enum Field {
    Value,
    P50,
    P99,
}

#[derive(Debug, Clone, Copy)]
enum Better {
    Higher,
    Lower,
}

/// One compared metric. `local` marks the same-pod baseline twins: a
/// move there flags hardware/kernel drift (`baseline-drift`), not a
/// FUSE regression.
struct Spec {
    phase: &'static str,
    metric: &'static str,
    field: Field,
    better: Better,
    /// Relative threshold (fraction) before `--threshold-scale`.
    threshold: f64,
    local: bool,
}

/// Defaults: throughput/p50 ±10%; p99 ±30%; cold phases ±30%;
/// ratios ±15%.
const SPECS: &[Spec] = &[
    Spec {
        phase: "read_storm_cold",
        metric: "mib_s",
        field: Field::Value,
        better: Better::Higher,
        threshold: 0.30,
        local: false,
    },
    Spec {
        phase: "read_storm_cold",
        metric: "open_ns",
        field: Field::P99,
        better: Better::Lower,
        threshold: 0.30,
        local: false,
    },
    Spec {
        phase: "read_storm_warm",
        metric: "mib_s",
        field: Field::Value,
        better: Better::Higher,
        threshold: 0.10,
        local: false,
    },
    Spec {
        phase: "read_storm_warm",
        metric: "open_ns",
        field: Field::P99,
        better: Better::Lower,
        threshold: 0.30,
        local: false,
    },
    Spec {
        phase: "open_storm_pass2",
        metric: "open_ns",
        field: Field::P50,
        better: Better::Lower,
        threshold: 0.10,
        local: false,
    },
    Spec {
        phase: "open_storm_pass2",
        metric: "open_ns",
        field: Field::P99,
        better: Better::Lower,
        threshold: 0.30,
        local: false,
    },
    Spec {
        phase: "randread_warm",
        metric: "io_ns",
        field: Field::P50,
        better: Better::Lower,
        threshold: 0.10,
        local: false,
    },
    Spec {
        phase: "randread_warm",
        metric: "io_ns",
        field: Field::P99,
        better: Better::Lower,
        threshold: 0.30,
        local: false,
    },
    Spec {
        phase: "randread_warm",
        metric: "iops",
        field: Field::Value,
        better: Better::Higher,
        threshold: 0.10,
        local: false,
    },
    Spec {
        phase: "randread_cold",
        metric: "io_ns",
        field: Field::P99,
        better: Better::Lower,
        threshold: 0.30,
        local: false,
    },
    Spec {
        phase: "read_storm_local_warm",
        metric: "mib_s",
        field: Field::Value,
        better: Better::Higher,
        threshold: 0.10,
        local: true,
    },
    Spec {
        phase: "randread_local_warm",
        metric: "iops",
        field: Field::Value,
        better: Better::Higher,
        threshold: 0.10,
        local: true,
    },
    Spec {
        phase: "ratios",
        metric: "warm_read_slowdown",
        field: Field::Value,
        better: Better::Lower,
        threshold: 0.15,
        local: false,
    },
    Spec {
        phase: "ratios",
        metric: "randread_warm_slowdown",
        field: Field::Value,
        better: Better::Lower,
        threshold: 0.15,
        local: false,
    },
    // jq_build compile phases: cold gets the cold-class threshold
    // (store-RTT variance), warm the tight one.
    Spec {
        phase: "jq_build_cold",
        metric: "total_wall_ms",
        field: Field::Value,
        better: Better::Lower,
        threshold: 0.30,
        local: false,
    },
    Spec {
        phase: "jq_build_warm",
        metric: "total_wall_ms",
        field: Field::Value,
        better: Better::Lower,
        threshold: 0.10,
        local: false,
    },
];

#[derive(Debug)]
pub struct Verdict {
    pub path: String,
    pub baseline: Option<f64>,
    pub current: Option<f64>,
    pub change_pct: Option<f64>,
    /// `improved` | `regressed` | `within-noise` | `inconclusive` |
    /// `missing` | `baseline-drift`; suffixed ` untrusted` under
    /// `--force`.
    pub verdict: String,
    pub threshold_pct: f64,
}

impl fmt::Display for Verdict {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let num = |v: Option<f64>| v.map_or("-".to_string(), |v| format!("{v:.1}"));
        let pct = self
            .change_pct
            .map_or(String::new(), |p| format!("  ({p:+.1}%)"));
        write!(
            f,
            "{:<36} {:>10} -> {:>10}{pct}  {}(±{:.0}%)",
            self.path,
            num(self.baseline),
            num(self.current),
            self.verdict,
            self.threshold_pct
        )
    }
}

pub struct Outcome {
    pub verdicts: Vec<Verdict>,
    /// Identity-key mismatches; non-empty = refusal (unless forced).
    pub refusals: Vec<String>,
    pub forced: bool,
}

impl Outcome {
    pub fn exit_code(&self) -> i32 {
        if !self.refusals.is_empty() && !self.forced {
            return EXIT_REFUSED;
        }
        if self
            .verdicts
            .iter()
            .any(|v| v.verdict.starts_with("regressed"))
        {
            return EXIT_REGRESSED;
        }
        0
    }
}

/// Identity key: every mismatch is reported (not just the first) so
/// one compare failure tells the operator the full distance between
/// the two runs.
fn identity_mismatches(a: &ResultV1, b: &ResultV1) -> Vec<String> {
    fn diff(out: &mut Vec<String>, what: &str, x: &str, y: &str) {
        if x != y {
            out.push(format!("{what}: {x:?} vs {y:?}"));
        }
    }
    let mut out = Vec::new();
    diff(&mut out, "schema", &a.schema, &b.schema);
    diff(
        &mut out,
        "workload.version",
        &a.workload.version.to_string(),
        &b.workload.version.to_string(),
    );
    diff(
        &mut out,
        "workload.dataset_digest",
        &a.workload.dataset_digest,
        &b.workload.dataset_digest,
    );
    // jq source / toolchain identity: a bump to either changes what
    // the compile phases measure. None==None (both skipped) compares.
    diff(
        &mut out,
        "workload.jq_src",
        a.workload.jq_src.as_deref().unwrap_or("-"),
        b.workload.jq_src.as_deref().unwrap_or("-"),
    );
    diff(
        &mut out,
        "workload.toolchain",
        a.workload.toolchain.as_deref().unwrap_or("-"),
        b.workload.toolchain.as_deref().unwrap_or("-"),
    );
    {
        let (mut pa, mut pb) = (a.workload.phases.clone(), b.workload.phases.clone());
        pa.sort();
        pb.sort();
        if pa != pb {
            out.push("phase sets differ".into());
        }
    }
    diff(
        &mut out,
        "reps",
        &a.workload.reps.to_string(),
        &b.workload.reps.to_string(),
    );
    diff(
        &mut out,
        "instance_type",
        a.placement.instance_type.as_deref().unwrap_or("?"),
        b.placement.instance_type.as_deref().unwrap_or("?"),
    );
    diff(
        &mut out,
        "kernel (major.minor)",
        &kernel_mm(&a.placement.kernel),
        &kernel_mm(&b.placement.kernel),
    );
    for (side, r) in [("baseline", a), ("current", b)] {
        if r.placement.attribution != "exact" {
            out.push(format!("{side} run is unattributed — never comparable"));
        }
    }
    if a.placement.contended != b.placement.contended {
        out.push(format!(
            "concurrency tag differs: baseline contended={} vs current contended={}",
            a.placement.contended, b.placement.contended
        ));
    }
    out
}

fn kernel_mm(kernel: &str) -> String {
    kernel.split('.').take(2).collect::<Vec<_>>().join(".")
}

fn pick(r: &ResultV1, s: &Spec) -> (Option<f64>, Option<f64>) {
    let m = r.phases.get(s.phase).and_then(|p| p.metrics.get(s.metric));
    let val = m.and_then(|m| match s.field {
        Field::Value => m.value,
        Field::P50 => m.p50,
        Field::P99 => m.p99,
    });
    (val, m.and_then(|m| m.rep_spread))
}

/// `a` = baseline side, `b` = current.
pub fn compare(a: &ResultV1, b: &ResultV1, force: bool, threshold_scale: f64) -> Outcome {
    let refusals = identity_mismatches(a, b);
    let mut verdicts = Vec::new();
    if refusals.is_empty() || force {
        for s in SPECS {
            let field_name = match s.field {
                Field::Value => String::new(),
                Field::P50 => "_p50".into(),
                Field::P99 => "_p99".into(),
            };
            let path = format!("{}.{}{field_name}", s.phase, s.metric);
            let threshold = s.threshold * threshold_scale;
            let (base, spread_b) = pick(a, s);
            let (cur, spread_c) = pick(b, s);
            let verdict = match (base, cur) {
                (Some(base), Some(cur)) if base != 0.0 => {
                    let rel = (cur - base) / base;
                    let worse = match s.better {
                        Better::Higher => rel < -threshold,
                        Better::Lower => rel > threshold,
                    };
                    let better = match s.better {
                        Better::Higher => rel > threshold,
                        Better::Lower => rel < -threshold,
                    };
                    // In-run noise gates the verdict: a metric whose
                    // rep_spread exceeds its own regression threshold
                    // can't convict (or acquit) anything.
                    let noisy = spread_b.is_some_and(|s| s > threshold)
                        || spread_c.is_some_and(|s| s > threshold);
                    let v = if worse && noisy {
                        "inconclusive(noisy)"
                    } else if worse && s.local {
                        // The local baseline moving = the NODE moved
                        // (hardware/kernel/EBS drift), not the FUSE.
                        "baseline-drift"
                    } else if worse {
                        "regressed"
                    } else if better && s.local {
                        "baseline-drift"
                    } else if better {
                        "improved"
                    } else {
                        "within-noise"
                    };
                    Verdict {
                        path,
                        baseline: Some(base),
                        current: Some(cur),
                        change_pct: Some(rel * 100.0),
                        verdict: v.into(),
                        threshold_pct: threshold * 100.0,
                    }
                }
                // Both values present but the baseline is zero: a
                // relative change is undefined, yet "missing" would
                // misreport data that exists. Non-fatal — exit codes
                // only key off `regressed`.
                (Some(base), Some(cur)) => Verdict {
                    path,
                    baseline: Some(base),
                    current: Some(cur),
                    change_pct: None,
                    verdict: "not-comparable(zero-baseline)".into(),
                    threshold_pct: threshold * 100.0,
                },
                _ => Verdict {
                    path,
                    baseline: base,
                    current: cur,
                    change_pct: None,
                    verdict: "missing".into(),
                    threshold_pct: threshold * 100.0,
                },
            };
            verdicts.push(verdict);
        }
    }
    if force && !refusals.is_empty() {
        for v in &mut verdicts {
            v.verdict.push_str(" untrusted");
        }
    }
    Outcome {
        verdicts,
        refusals,
        forced: force,
    }
}

#[cfg(test)]
mod tests {
    use super::super::baseline::tests::eligible;
    use super::*;

    fn set(r: &mut ResultV1, phase: &str, metric: &str, field: &str, v: f64) {
        let m = r
            .phases
            .get_mut(phase)
            .unwrap()
            .metrics
            .get_mut(metric)
            .unwrap();
        match field {
            "value" => m.value = Some(v),
            "p99" => m.p99 = Some(v),
            "spread" => m.rep_spread = Some(v),
            _ => unreachable!(),
        }
    }

    #[test]
    fn within_noise_then_regressed_then_improved() {
        let base = eligible();

        // -2.5% on a ±10% throughput metric → within-noise.
        let mut cur = eligible();
        set(&mut cur, "read_storm_warm", "mib_s", "value", 975.0);
        let o = compare(&base, &cur, false, 1.0);
        let v = o
            .verdicts
            .iter()
            .find(|v| v.path == "read_storm_warm.mib_s")
            .unwrap();
        assert_eq!(v.verdict, "within-noise");
        assert_eq!(o.exit_code(), 0);

        // -20% throughput → regressed, exit 3.
        let mut cur = eligible();
        set(&mut cur, "read_storm_warm", "mib_s", "value", 800.0);
        let o = compare(&base, &cur, false, 1.0);
        let v = o
            .verdicts
            .iter()
            .find(|v| v.path == "read_storm_warm.mib_s")
            .unwrap();
        assert_eq!(v.verdict, "regressed");
        assert_eq!(o.exit_code(), EXIT_REGRESSED);

        // +20% → improved; still exit 0.
        let mut cur = eligible();
        set(&mut cur, "read_storm_warm", "mib_s", "value", 1200.0);
        let o = compare(&base, &cur, false, 1.0);
        assert_eq!(
            o.verdicts
                .iter()
                .find(|v| v.path == "read_storm_warm.mib_s")
                .unwrap()
                .verdict,
            "improved"
        );
        assert_eq!(o.exit_code(), 0);
    }

    #[test]
    fn noisy_metric_cannot_convict() {
        let base = eligible();
        let mut cur = eligible();
        // A 20% drop that would be `regressed` — but the current run's
        // own rep_spread (25%) exceeds the 10% threshold, so the
        // verdict must be inconclusive, never a false conviction.
        set(&mut cur, "read_storm_warm", "mib_s", "value", 800.0);
        set(&mut cur, "read_storm_warm", "mib_s", "spread", 0.25);
        let o = compare(&base, &cur, false, 1.0);
        let v = o
            .verdicts
            .iter()
            .find(|v| v.path == "read_storm_warm.mib_s")
            .unwrap();
        assert_eq!(v.verdict, "inconclusive(noisy)");
        assert_eq!(o.exit_code(), 0);
    }

    #[test]
    fn ratio_regression_uses_lower_is_better() {
        let base = eligible();
        let mut cur = eligible();
        // Slowdown ratio 1.2 → 1.5 (+25% > ±15%): the mount got slower
        // relative to local disk → regressed.
        set(&mut cur, "ratios", "warm_read_slowdown", "value", 1.5);
        let o = compare(&base, &cur, false, 1.0);
        assert_eq!(
            o.verdicts
                .iter()
                .find(|v| v.path == "ratios.warm_read_slowdown")
                .unwrap()
                .verdict,
            "regressed"
        );
    }

    #[test]
    fn identity_mismatch_refuses_with_reasons() {
        let base = eligible();

        let mut cur = eligible();
        cur.placement.instance_type = Some("c6a.4xlarge".into());
        let o = compare(&base, &cur, false, 1.0);
        assert_eq!(o.exit_code(), EXIT_REFUSED);
        assert!(o.refusals.iter().any(|r| r.contains("instance_type")));
        assert!(o.verdicts.is_empty(), "no verdicts on refusal");

        let mut cur = eligible();
        cur.placement.contended = true;
        let o = compare(&base, &cur, false, 1.0);
        assert!(o.refusals.iter().any(|r| r.contains("concurrency tag")));

        let mut cur = eligible();
        cur.placement.attribution = "unattributed".into();
        let o = compare(&base, &cur, false, 1.0);
        assert!(o.refusals.iter().any(|r| r.contains("unattributed")));

        // Kernel patch level may differ; major.minor must match.
        let mut cur = eligible();
        cur.placement.kernel = "6.12.99".into();
        assert_eq!(compare(&base, &cur, false, 1.0).exit_code(), 0);
        cur.placement.kernel = "6.13.0".into();
        assert_eq!(compare(&base, &cur, false, 1.0).exit_code(), EXIT_REFUSED);

        // Identity: a different dataset (harvest or layout change)
        // and a toolchain bump each refuse — the numbers measure
        // different work.
        let mut cur = eligible();
        cur.workload.dataset_digest = "other-digest".into();
        let o = compare(&base, &cur, false, 1.0);
        assert!(o.refusals.iter().any(|r| r.contains("dataset_digest")));

        let mut cur = eligible();
        cur.workload.toolchain = Some("hash-gcc-wrapper-15".into());
        let o = compare(&base, &cur, false, 1.0);
        assert!(o.refusals.iter().any(|r| r.contains("toolchain")));

        let mut cur = eligible();
        cur.workload.jq_src = None;
        let o = compare(&base, &cur, false, 1.0);
        assert!(
            o.refusals.iter().any(|r| r.contains("jq_src")),
            "jq present vs skipped is not comparable"
        );
    }

    #[test]
    fn zero_baseline_is_not_comparable_not_missing() {
        let mut base = eligible();
        base.phases
            .get_mut("read_storm_warm")
            .unwrap()
            .metrics
            .get_mut("mib_s")
            .unwrap()
            .value = Some(0.0);
        let cur = eligible();
        let o = compare(&base, &cur, false, 1.0);
        let v = o
            .verdicts
            .iter()
            .find(|v| v.path == "read_storm_warm.mib_s")
            .unwrap();
        // Both sides have data — calling it "missing" would be a lie;
        // a zero denominator just has no defined relative change.
        assert_eq!(v.verdict, "not-comparable(zero-baseline)");
        assert_eq!(v.baseline, Some(0.0));
        assert!(v.current.is_some());
        assert_eq!(o.exit_code(), 0);
    }

    #[test]
    fn force_marks_verdicts_untrusted() {
        let base = eligible();
        let mut cur = eligible();
        cur.placement.instance_type = Some("c6a.4xlarge".into());
        let o = compare(&base, &cur, true, 1.0);
        assert_ne!(o.exit_code(), EXIT_REFUSED, "force bypasses refusal");
        assert!(!o.verdicts.is_empty());
        assert!(o.verdicts.iter().all(|v| v.verdict.ends_with("untrusted")));
    }

    #[test]
    fn threshold_scale_widens_all_gates() {
        let base = eligible();
        let mut cur = eligible();
        // -20% is regressed at scale 1.0 (±10%) but within-noise at
        // scale 3.0 (±30%).
        set(&mut cur, "read_storm_warm", "mib_s", "value", 800.0);
        assert_eq!(compare(&base, &cur, false, 1.0).exit_code(), EXIT_REGRESSED);
        assert_eq!(compare(&base, &cur, false, 3.0).exit_code(), 0);
    }

    #[test]
    fn local_twin_drift_is_not_a_fuse_regression() {
        let base = eligible();
        let mut cur = eligible();
        // The LOCAL baseline twin moved 20% — node drift, not FUSE.
        cur.phases
            .get_mut("read_storm_warm")
            .unwrap()
            .metrics
            .insert(
                "mib_s".into(),
                super::super::result::Metric {
                    unit: "mib_s".into(),
                    value: Some(1000.0),
                    ..Default::default()
                },
            );
        cur.phases.insert(
            "read_storm_local_warm".into(),
            super::super::result::PhaseMetrics {
                start_epoch_ms: 0,
                end_epoch_ms: 1,
                reps: 3,
                metrics: std::collections::BTreeMap::from([(
                    "mib_s".to_string(),
                    super::super::result::Metric {
                        unit: "mib_s".into(),
                        value: Some(800.0),
                        ..Default::default()
                    },
                )]),
            },
        );
        let mut b2 = base.clone();
        b2.phases.insert(
            "read_storm_local_warm".into(),
            cur.phases["read_storm_local_warm"].clone(),
        );
        let b2_local = b2.phases.get_mut("read_storm_local_warm").unwrap();
        b2_local.metrics.get_mut("mib_s").unwrap().value = Some(1000.0);
        let o = compare(&b2, &cur, false, 1.0);
        let v = o
            .verdicts
            .iter()
            .find(|v| v.path == "read_storm_local_warm.mib_s")
            .unwrap();
        assert_eq!(v.verdict, "baseline-drift");
        assert_eq!(o.exit_code(), 0, "baseline-drift never exits nonzero");
    }
}
