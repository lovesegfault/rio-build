//! `xtask k8s sla-gates` — ADR-023 §Phasing-13a empirical-gate harness.
//!
//! Operator-run on a live EKS cluster; NOT a CI test. The four gates
//! are GO/NO-GO criteria for enabling cost-solve in production:
//!
//! - **a**: cross-h α recovery on a c↔h-correlated probe set. Seeds
//!   synthetic `build_samples` with known ground-truth α, waits one
//!   refit, asserts the recovered α (via `export-corpus`) is within
//!   0.05 per dimension.
//! - **b**: rank-K residual bias spread ≤ τ. Queries `build_samples`
//!   for organic pnames with ≥5 samples on ≥3 hw_classes, computes
//!   per-hw_class median log-residual against the cached fit, reports
//!   max−min spread per pname.
//! - **c**: per-phase relative error ≤ 0.15. Queries the
//!   `rio_scheduler_sla_prediction_ratio` histogram and reports
//!   per-dimension p50/p90 of |ratio−1|. Manual interpretation
//!   (chromium-shaped workloads need a held-out set the harness can't
//!   pick).
//! - **d**: realized envelope-miss rate ≤ 1.2·(1−q) per tier. Scrapes
//!   `rio_scheduler_sla_envelope_result_total`, reports miss/(hit+miss)
//!   per tier alongside the 1.2·(1−q) bound for q∈{0.5,0.9,0.99}.
//!
//! Gates a/b assert; c/d report only (manual interpretation against
//! the operator's tier ladder + workload).

use std::collections::BTreeMap;
use std::time::Duration;

use anyhow::{Context, Result, bail, ensure};
use clap::ValueEnum;
use serde_json::Value;
use sqlx::Row;

use crate::k8s::eks::smoke::CliCtx;
use crate::k8s::qa::ctx::PgHandle;
use crate::k8s::status::{SCHED_METRICS_PORT, Scrape, scrape_pod};
use crate::k8s::{NS, client as kube};
use crate::ui;

#[derive(Debug, Clone, Copy, ValueEnum)]
pub enum Gate {
    /// Cross-h α recovery on c↔h-correlated synthetic probe (asserts).
    A,
    /// Rank-K residual bias spread on organic samples (asserts ≤ τ).
    B,
    /// Per-dimension prediction-ratio quantiles (reports only).
    C,
    /// Per-tier envelope-miss rate vs 1.2·(1−q) (reports only).
    D,
}

/// gate-a synthetic pname. Namespaced so `sla reset` cleanup can't hit
/// a real pname.
const GATE_A_PNAME: &str = "xtask-sla-gate-a";

/// gate-a per-dimension tolerance on |α̂_d − α_d|.
const GATE_A_TOL: f64 = 0.05;

/// gate-b spread threshold τ on per-hw_class median log-residual
/// (max−min across hw_classes for one pname). 0.35 ≈ a 1.42× wall-
/// clock bias gap between the best- and worst-fitting hw_class — wider
/// than the 0.25 σ noise-floor the hw-normalize VM subtest tolerates,
/// tighter than the 0.4 ln-2 gap normalization is meant to close.
const GATE_B_TAU: f64 = 0.35;

pub async fn run(gate: Gate) -> Result<()> {
    let kube = kube::Client::try_default()
        .await
        .context("kube client (is KUBECONFIG set / `up --kubeconfig` run?)")?;
    let cli = CliCtx::open(&kube, 0, 0).await?;
    let pg = PgHandle::open(&kube).await?;
    match gate {
        Gate::A => gate_a(&cli, &pg).await,
        Gate::B => gate_b(&cli, &pg).await,
        Gate::C => gate_c(&kube).await,
        Gate::D => gate_d(&kube).await,
    }
}

// ─── gate-a: cross-h α recovery ────────────────────────────────────────

/// Seed a c↔h-correlated probe set with known ground-truth α, wait one
/// refit, read α back via `export-corpus`, assert per-dim error < 0.05.
///
/// "c↔h-correlated" = each `cpu_limit_cores` value lands on exactly one
/// `hw_class` — the confound the §13a alpha-ALS rank gate is meant to
/// resolve. The probe is pure-ALU (`α=[1,0,0]`) so wall scales by the
/// per-h `alu` factor only; `membw`/`ioseq` factors are seeded ≠1 so a
/// recovered α leaning on those dimensions shows up as gate failure.
async fn gate_a(cli: &CliCtx, pg: &PgHandle) -> Result<()> {
    // ── ground truth ────────────────────────────────────────────────
    // K=3 dims (alu, membw, ioseq). α=[1,0,0] (pure compute). The
    // per-h factor vectors must be pairwise non-collinear after
    // centring (rank gate, alpha.rs THETA_MIN_COS) — pick membw/ioseq
    // off-axis so the design matrix has rank 3.
    let truth: [f64; 3] = [1.0, 0.0, 0.0];
    let hw: [(&str, [f64; 3]); 3] = [
        ("xtask-gate-a-h0", [1.0, 0.6, 1.4]),
        ("xtask-gate-a-h1", [1.6, 1.3, 0.7]),
        ("xtask-gate-a-h2", [2.4, 0.8, 1.1]),
    ];
    // T_ref(c) = S + P/c with S=30 P=2000 (the curve every other SLA
    // fixture uses). c↔h pairing: each h gets two c-values, no overlap.
    let probe: [(f64, usize); 6] = [
        (4.0, 0),
        (8.0, 0),
        (12.0, 1),
        (16.0, 1),
        (32.0, 2),
        (64.0, 2),
    ];

    ui::step("seed hw_perf_samples + build_samples", || async {
        // Wipe prior gate-a state so re-runs are idempotent.
        sqlx::query("DELETE FROM hw_perf_samples WHERE hw_class LIKE 'xtask-gate-a-%'")
            .execute(&pg.pool)
            .await?;
        cli.run(&["sla", "reset", GATE_A_PNAME])?;
        // ≥FLEET_MEDIAN_MIN_TENANTS distinct submitting_tenant per h
        // so cross_tenant_median admits each (per-dim trust gate pins
        // factor=[1.0;K] otherwise — REVIEW.md §Stability-tests).
        // bug_016: const-ref so a future gate change is a compile
        // error here, not a silent rank-0 α-ALS false-FAIL.
        for (h, f) in &hw {
            for i in 0..rio_scheduler::sla::FLEET_MEDIAN_MIN_TENANTS {
                sqlx::query(
                    "INSERT INTO hw_perf_samples (hw_class, pod_id, factor, submitting_tenant) \
                     VALUES ($1, $2, jsonb_build_object('alu',$3::float8,'membw',$4::float8,'ioseq',$5::float8), $6) \
                     ON CONFLICT (hw_class, pod_id) DO UPDATE SET factor = EXCLUDED.factor",
                )
                .bind(h).bind(format!("xtask-gate-a-{h}-{i}"))
                .bind(f[0]).bind(f[1]).bind(f[2])
                .bind(format!("xtask-t{i}"))
                .execute(&pg.pool).await?;
            }
        }
        // wall_obs = T_ref(c) / (α·factor[h]) = (30 + 2000/c) / alu[h].
        for &(c, hi) in &probe {
            let (h, f) = &hw[hi];
            let t_ref = 30.0 + 2000.0 / c;
            let speedup: f64 = truth.iter().zip(f).map(|(a, k)| a * k).sum();
            let wall = t_ref / speedup;
            sqlx::query(
                "INSERT INTO build_samples \
                   (pname, system, tenant, duration_secs, peak_memory_bytes, \
                    cpu_limit_cores, cpu_seconds_total, hw_class, completed_at) \
                 VALUES ($1, 'x86_64-linux', '', $2, 1073741824, $3, $4, $5, now())",
            )
            .bind(GATE_A_PNAME)
            .bind(wall)
            .bind(c)
            .bind(wall * c * 0.9)
            .bind(h)
            .execute(&pg.pool)
            .await?;
        }
        anyhow::Ok(())
    })
    .await?;

    // One estimator refresh: SlaStatus.hasFit flips true once the
    // refit picked up the rows. ESTIMATOR_REFRESH_EVERY × tickInterval
    // is ≤60s on every deploy profile.
    ui::poll("estimator refit", Duration::from_secs(5), 24, || async {
        let out = cli.run(&["--json", "sla", "status", GATE_A_PNAME])?;
        let v: Value = serde_json::from_str(&out)?;
        Ok(v.get("hasFit")
            .and_then(Value::as_bool)
            .unwrap_or(false)
            .then_some(()))
    })
    .await?;

    // export-corpus is the only RPC surface that carries α.
    let alpha = ui::step("read recovered α via export-corpus", || async {
        let tmp = tempfile::NamedTempFile::new()?;
        let path = tmp.path().to_str().context("tempfile path utf8")?;
        cli.run(&["sla", "export-corpus", "--min-n", "1", "-o", path])?;
        let body = std::fs::read_to_string(tmp.path())?;
        let corpus: Value =
            serde_json::from_str(&body).with_context(|| format!("parse corpus json: {body}"))?;
        let entries = corpus
            .get("entries")
            .and_then(Value::as_array)
            .context("corpus.entries[]")?;
        let entry = entries
            .iter()
            .find(|e| e.get("pname").and_then(Value::as_str) == Some(GATE_A_PNAME))
            .with_context(|| format!("{GATE_A_PNAME} not in corpus ({} entries)", entries.len()))?;
        let alpha: Vec<f64> = entry
            .get("alpha")
            .and_then(Value::as_array)
            .map(|a| a.iter().filter_map(Value::as_f64).collect())
            .unwrap_or_default();
        anyhow::Ok(alpha)
    })
    .await?;

    println!("\ngate-a  α recovery (c↔h-correlated probe, ground-truth α={truth:?})");
    println!("  recovered α = {alpha:?}");
    ensure!(
        alpha.len() == 3,
        "recovered α has K={} (expected 3) — alpha-ALS rank gate never opened? \
         Check sla.hwClasses includes xtask-gate-a-h{{0,1,2}} or the corpus \
         exported the v1 (alpha-less) shape.",
        alpha.len()
    );
    let mut pass = true;
    for d in 0..3 {
        let err = (alpha[d] - truth[d]).abs();
        let ok = err <= GATE_A_TOL;
        pass &= ok;
        println!(
            "  α[{d}]: got {:.4}  truth {:.1}  |Δ|={:.4}  {}",
            alpha[d],
            truth[d],
            err,
            if ok { "ok" } else { "FAIL" }
        );
    }
    ensure!(
        pass,
        "gate-a FAIL: |α̂−α| > {GATE_A_TOL} on at least one dim"
    );
    println!("gate-a  PASS");
    Ok(())
}

// ─── gate-b: rank-K residual bias spread ───────────────────────────────

/// For every organic pname with ≥5 samples spread over ≥3 hw_classes,
/// compute per-hw_class median log-residual against the cached fit's
/// reference-second curve and report max−min spread. Spread > τ means
/// the K=3 mixture is leaving structured per-h bias on the table.
async fn gate_b(cli: &CliCtx, pg: &PgHandle) -> Result<()> {
    // Candidate pnames: ≥5 samples over ≥3 distinct hw_classes in the
    // last 7d (same window HwTable reads). Exclude gate-a's synthetic.
    let pnames: Vec<(String, String)> = sqlx::query(
        "SELECT pname, system FROM build_samples \
           WHERE completed_at > now() - interval '7 days' \
             AND hw_class IS NOT NULL AND NOT outlier_excluded \
             AND pname NOT LIKE 'xtask-%' \
         GROUP BY pname, system \
           HAVING count(*) >= 5 AND count(DISTINCT hw_class) >= 3 \
         ORDER BY count(*) DESC LIMIT 50",
    )
    .map(|r: sqlx::postgres::PgRow| (r.get("pname"), r.get("system")))
    .fetch_all(&pg.pool)
    .await?;

    if pnames.is_empty() {
        bail!(
            "gate-b: no pname with ≥5 hw-tagged samples on ≥3 hw_classes in the \
             last 7d. Let organic traffic accrue (or seed via gate-a) first."
        );
    }

    println!("\ngate-b  per-hw_class median log-residual spread (τ={GATE_B_TAU})");
    println!(
        "  {:<32} {:>6} {:>4}  {:>8}  per-hw_class median ln(obs/T_ref(c))",
        "pname", "n", "n_h", "spread"
    );
    let mut worst = 0.0f64;
    let mut over = 0usize;
    for (pname, system) in &pnames {
        // T_ref(c) via `DurationFit::t_at` — do NOT re-derive the curve
        // here (bug_032: open-coded `s+p/c+q·c` missed the p̄ clamp on
        // Capped/Usl fits → spurious per-h spread → false-FAIL).
        // `gate_b_t_ref` returns `None` for has_fit=false / Probe.
        let out = cli.run(&["--json", "sla", "status", pname, "--system", system])?;
        let st: Value = serde_json::from_str(&out)?;

        let rows: Vec<(String, f64, f64)> = sqlx::query(
            "SELECT hw_class, duration_secs, cpu_limit_cores FROM build_samples \
               WHERE pname=$1 AND system=$2 AND hw_class IS NOT NULL \
                 AND cpu_limit_cores IS NOT NULL AND NOT outlier_excluded \
                 AND completed_at > now() - interval '7 days'",
        )
        .bind(pname)
        .bind(system)
        .map(|r: sqlx::postgres::PgRow| {
            (
                r.get("hw_class"),
                r.get("duration_secs"),
                r.get("cpu_limit_cores"),
            )
        })
        .fetch_all(&pg.pool)
        .await?;

        // Per-hw_class median of ln(wall_obs / T_ref(c)). A perfectly
        // hw-normalized fit has median ≈ −ln(α·factor[h]) per h; the
        // SPREAD across h is what gate-b bounds (rank-K bias structure).
        let mut by_h: BTreeMap<String, Vec<f64>> = BTreeMap::new();
        for (h, wall, c) in &rows {
            let Some(pred) = gate_b_t_ref(&st, *c) else {
                continue;
            };
            if pred > 0.0 && *wall > 0.0 {
                by_h.entry(h.clone()).or_default().push((wall / pred).ln());
            }
        }
        let medians: Vec<(String, f64)> = by_h
            .into_iter()
            .filter(|(_, v)| !v.is_empty())
            .map(|(h, mut v)| {
                v.sort_by(|a, b| a.total_cmp(b));
                (h, v[v.len() / 2])
            })
            .collect();
        if medians.len() < 3 {
            continue;
        }
        let lo = medians
            .iter()
            .map(|(_, m)| *m)
            .fold(f64::INFINITY, f64::min);
        let hi = medians
            .iter()
            .map(|(_, m)| *m)
            .fold(f64::NEG_INFINITY, f64::max);
        let spread = hi - lo;
        worst = worst.max(spread);
        if spread > GATE_B_TAU {
            over += 1;
        }
        let cells: String = medians
            .iter()
            .map(|(h, m)| format!("{h}={m:+.3}"))
            .collect::<Vec<_>>()
            .join(" ");
        println!(
            "  {:<32} {:>6} {:>4}  {:>8.4}{} {cells}",
            truncate(pname, 32),
            rows.len(),
            medians.len(),
            spread,
            if spread > GATE_B_TAU { "*" } else { " " }
        );
    }
    println!(
        "\n  worst spread = {worst:.4}  (τ={GATE_B_TAU}, {over}/{} over)",
        pnames.len()
    );
    ensure!(
        worst <= GATE_B_TAU,
        "gate-b FAIL: worst per-hw_class bias spread {worst:.4} > τ={GATE_B_TAU}"
    );
    println!("gate-b  PASS");
    Ok(())
}

/// gate-b's T_ref(c) — the cached fit's reference-second curve at `c`.
/// `None` ⇔ no fit / Probe (no curve to compare against).
///
/// Goes through the typed `SlaStatusResponse` →
/// [`duration_fit_from_status`] → `DurationFit::t_at` so the curve has
/// ONE source. Do NOT re-derive `s + p/c + q·c` here — bug_032: that
/// form misses the p̄ clamp on Capped/Usl, under-predicts at `c > p̄`
/// by `c/p̄`, and projects as spurious per-h spread → false-FAIL.
///
/// [`duration_fit_from_status`]: rio_scheduler::admin::duration_fit_from_status
fn gate_b_t_ref(st: &serde_json::Value, c: f64) -> Option<f64> {
    use rio_scheduler::sla::types::RawCores;
    let st: rio_proto::types::SlaStatusResponse = serde_json::from_value(st.clone()).ok()?;
    if !st.has_fit {
        return None;
    }
    rio_scheduler::admin::duration_fit_from_status(&st).map(|f| f.t_at(RawCores(c)).0)
}

// ─── gate-c: per-dimension prediction-ratio (report only) ──────────────

async fn gate_c(kube: &kube::Client) -> Result<()> {
    let scrape = scrape_leader(kube).await?;
    println!("\ngate-c  prediction-ratio |actual/predicted − 1| (target ≤ 0.15; manual)");
    println!(
        "  {:<6} {:>10} {:>10} {:>10} {:>10}",
        "dim", "count", "sum", "mean", "|mean−1|"
    );
    let mut any = false;
    for dim in ["wall", "mem"] {
        // metrics-exporter-prometheus emits histograms as `_sum`/
        // `_count` (+ buckets). Mean ratio is the cheapest summary the
        // harness can compute without a Prometheus query API; per-phase
        // breakdown needs a held-out set the operator picks.
        let count = scrape
            .labeled("rio_scheduler_sla_prediction_ratio_count", "dim", dim)
            .unwrap_or(0.0);
        let sum = scrape
            .labeled("rio_scheduler_sla_prediction_ratio_sum", "dim", dim)
            .unwrap_or(0.0);
        if count == 0.0 {
            println!("  {dim:<6} {:>10} (no samples)", 0);
            continue;
        }
        any = true;
        let mean = sum / count;
        let rel = (mean - 1.0).abs();
        println!(
            "  {dim:<6} {count:>10.0} {sum:>10.1} {mean:>10.4} {rel:>10.4}{}",
            if rel <= 0.15 { "" } else { "  > 0.15" }
        );
    }
    if !any {
        bail!(
            "gate-c: rio_scheduler_sla_prediction_ratio has no samples — no completed builds with a fit yet"
        );
    }
    println!(
        "\n  (report only — per-phase / chromium-shaped breakdown needs a held-out \
         set; pick pnames and query build_samples directly)"
    );
    Ok(())
}

// ─── gate-d: envelope-miss rate vs 1.2·(1−q) (report only) ─────────────

async fn gate_d(kube: &kube::Client) -> Result<()> {
    let scrape = scrape_leader(kube).await?;
    // Group rio_scheduler_sla_envelope_result_total by (tier, result).
    let mut by_tier: BTreeMap<String, (f64, f64)> = BTreeMap::new();
    for (labels, v) in scrape.series("rio_scheduler_sla_envelope_result_total") {
        let Some(tier) = label_value(labels, "tier") else {
            continue;
        };
        let Some(result) = label_value(labels, "result") else {
            continue;
        };
        let e = by_tier.entry(tier).or_default();
        match result.as_str() {
            "hit" => e.0 += v,
            "miss" => e.1 += v,
            _ => {}
        }
    }
    if by_tier.is_empty() {
        bail!(
            "gate-d: rio_scheduler_sla_envelope_result_total has no series — \
             no envelope-evaluated builds yet"
        );
    }
    println!("\ngate-d  envelope-miss rate per tier (target ≤ 1.2·(1−q); manual)");
    println!(
        "  {:<16} {:>8} {:>8} {:>10}   bounds: q=.5→{:.3} q=.9→{:.3} q=.99→{:.3}",
        "tier",
        "hit",
        "miss",
        "miss_rate",
        1.2 * 0.5,
        1.2 * 0.1,
        1.2 * 0.01
    );
    for (tier, (hit, miss)) in &by_tier {
        let total = hit + miss;
        let rate = if total > 0.0 { miss / total } else { 0.0 };
        println!("  {tier:<16} {hit:>8.0} {miss:>8.0} {rate:>10.4}");
    }
    println!(
        "\n  (report only — match each tier's miss_rate against 1.2·(1−q) for that \
         tier's tightest configured bound: p50→q=.5, p90→q=.9, p99→q=.99)"
    );
    Ok(())
}

// ─── helpers ───────────────────────────────────────────────────────────

async fn scrape_leader(kube: &kube::Client) -> Result<Scrape> {
    let leader = kube::scheduler_leader(kube, NS).await?;
    let body = scrape_pod(kube, NS, &leader, SCHED_METRICS_PORT).await?;
    Ok(Scrape::parse(&body))
}

/// Extract `key="value"` from a Prometheus label string `{k="v",…}`.
fn label_value(labels: &str, key: &str) -> Option<String> {
    let needle = format!("{key}=\"");
    let i = labels.find(&needle)? + needle.len();
    let j = labels[i..].find('"')?;
    Some(labels[i..i + j].to_owned())
}

fn truncate(s: &str, n: usize) -> String {
    // bug_033: char-count, not byte-slice — `pname` is tenant-controlled
    // and the gateway char-clamps, so multi-byte reaches here. Byte-
    // slice at a non-char-boundary panics. (Column alignment is already
    // approximate for multi-byte since `{:<32}` pads by bytes.)
    if s.chars().count() <= n {
        s.to_owned()
    } else {
        format!("{}…", s.chars().take(n - 1).collect::<String>())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rio_proto::types::SlaStatusResponse;
    use rio_scheduler::sla::types::{DurationFit, RawCores, RefSeconds};

    #[test]
    fn label_value_extracts() {
        let l = r#"{tier="normal",result="miss",constraint="wall"}"#;
        assert_eq!(label_value(l, "tier").as_deref(), Some("normal"));
        assert_eq!(label_value(l, "result").as_deref(), Some("miss"));
        assert_eq!(label_value(l, "absent"), None);
    }

    /// bug_032: gate_b's T_ref(c) MUST equal the model's own
    /// `DurationFit::t_at(c)` — not a re-derived `s + p/c + q·c`. For
    /// `Capped{p̄=8}` at `c=32 > p̄`, the un-clamped form gives 92.5;
    /// the model gives 280. The gap projects as spurious per-h spread
    /// → false-FAIL `ensure!(worst <= GATE_B_TAU)`.
    #[test]
    fn gate_b_t_ref_matches_t_at() {
        // What `rio-cli --json sla status` would emit for a Capped fit
        // (snake_case — rio-cli serializes the prost struct via serde
        // directly, no protojson).
        let st = SlaStatusResponse {
            has_fit: true,
            fit_kind: "Capped".into(),
            s: 30.0,
            p: 2000.0,
            q: 0.0,
            p_bar: 8.0,
            ..Default::default()
        };
        let v = serde_json::to_value(&st).unwrap();
        let fit = DurationFit::Capped {
            s: RefSeconds(30.0),
            p: RefSeconds(2000.0),
            p_bar: RawCores(8.0),
        };
        let c = 32.0;
        let got = gate_b_t_ref(&v, c).unwrap();
        let want = fit.t_at(RawCores(c)).0;
        assert_eq!(
            got, want,
            "gate_b T_ref({c}) = {got} but DurationFit::t_at = {want} \
             (Capped p̄=8 clamp ignored?)"
        );
    }

    /// bug_033: `truncate` byte-slices `&s[..n-1]`; multi-byte chars at
    /// the boundary panic. `pname` is tenant-controlled and the gateway
    /// explicitly char-clamps (translate.rs `string_clamped` test seeds
    /// `"é".repeat(300)`).
    #[test]
    fn truncate_handles_multibyte_boundary() {
        let s = "é".repeat(20); // 20 chars / 40 bytes; byte 15 is mid-'é'
        let r = std::panic::catch_unwind(|| truncate(&s, 16));
        let out = r.expect("truncate must not panic on multi-byte input");
        // 15 'é' + '…' = 16 chars.
        assert_eq!(out.chars().count(), 16);
        assert!(out.ends_with('…'));
        // ASCII path unchanged.
        assert_eq!(truncate("hello", 32), "hello");
    }
}
