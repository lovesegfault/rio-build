//! `xtask k8s fsbench` — the castore-FUSE micro-benchmark driver
//! (P0594).
//!
//! `run` submits ONE bench build through the production ssh-ng path
//! (strictly serial — parallel benches would generate the contention
//! they then measure), samples co-tenancy and cluster metrics while it
//! runs, parses the PERF stream out of the build log, and writes
//! `.fsbench/{ts}/result.json` (schema fsbench/v1).
//!
//! Operator tooling, not CI: no checks.* entry, no wall-clock gates.

mod baseline;
mod coldreps;
mod compare;
mod cotenancy;
mod evict;
mod parse;
mod result;
mod submit;

use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result};
use rand::{RngExt, distr::Alphanumeric};
use tracing::{info, warn};

use super::eks::smoke::CliCtx;
use super::provider::{Provider, ProviderKind};
use crate::config::XtaskConfig;
use crate::sh::repo_root;
use result::ResultV1;

#[derive(clap::Args)]
pub struct FsbenchArgs {
    #[command(subcommand)]
    cmd: FsbenchCmd,
}

#[derive(clap::Subcommand)]
enum FsbenchCmd {
    /// Submit one bench build, sample co-tenancy, parse PERF lines,
    /// write .fsbench/{ts}/result.json, auto-compare against the
    /// baseline.
    Run(RunOpts),
    /// Compare two result files with the same engine the post-run
    /// auto-compare uses. A is the baseline side.
    Compare {
        a: PathBuf,
        b: PathBuf,
        /// Compare despite identity-key mismatches; every verdict is
        /// marked `untrusted`.
        #[arg(long)]
        force: bool,
        /// Multiply every regression threshold (e.g. 2.0 doubles all
        /// gates).
        #[arg(long, default_value_t = 1.0)]
        threshold_scale: f64,
    },
    /// Run N cold reps of the full bench, evicting the builder cache
    /// between each, and write `.fsbench/{ts}/cold-reps.json` with
    /// mean/stddev/stderr aggregates. Before/after-agnostic: it
    /// measures whatever image is deployed.
    ColdReps(ColdRepsOpts),
}

#[derive(clap::Args)]
struct ColdRepsOpts {
    /// Number of accepted cold reps to collect.
    #[arg(long, default_value_t = 5)]
    reps: u32,
    /// Dataset seed (fixed by default so the dataset drv is reused).
    #[arg(long, default_value = DEFAULT_SEED)]
    seed: String,
    /// Also write the aggregate to this path (e.g. before.json).
    #[arg(long, value_name = "FILE")]
    save: Option<PathBuf>,
    /// Cap on dropped reps (wrong node / dishonest cold / failed
    /// build) before giving up and emitting whatever was accepted.
    #[arg(long, default_value_t = 10)]
    max_redos: u32,
}

/// Default dataset seed — FIXED so the dataset drv is stable: the
/// ~1.9 GiB dataset is generated and uploaded once, then every later
/// run schedules it as a cache hit. The seed keys only the tree
/// layout (contents are pinned by flake.lock); cold-phase honesty is
/// the honesty gate's job, not the seed's. Bump the workload version
/// → bump this seed with it.
const DEFAULT_SEED: &str = "rio-fsbench-w1";

#[derive(clap::Args)]
struct RunOpts {
    /// Dataset seed. The default is fixed per workload version so the
    /// dataset is reused across runs; override to force a fresh
    /// dataset (e.g. after suspected dataset corruption).
    #[arg(long, default_value = DEFAULT_SEED)]
    seed: String,
    /// Save this run as `.fsbench/baselines/<NAME>.json` after it
    /// validates. Refused for contended / unattributed /
    /// dishonest-cold runs.
    #[arg(long, value_name = "NAME")]
    save_baseline: Option<String>,
    /// Baseline to auto-compare against (its absence is an info line,
    /// not an error).
    #[arg(long, default_value = "main")]
    baseline: String,
    /// Skip the post-run auto-compare.
    #[arg(long)]
    no_compare: bool,
    /// Compare despite identity-key mismatches (verdicts marked
    /// `untrusted`).
    #[arg(long)]
    force: bool,
    /// Multiply every regression threshold.
    #[arg(long, default_value_t = 1.0)]
    threshold_scale: f64,
}

/// Returns the process exit code: 0 ok, 2 = refusal (compare identity
/// mismatch OR a refused --save-baseline), 3 = ≥1 regressed metric
/// (operator scripting — fsbench is not CI-gated). A refused save
/// never downgrades a regression already in hand.
pub async fn run(
    args: FsbenchArgs,
    p: &dyn Provider,
    kind: ProviderKind,
    cfg: &XtaskConfig,
) -> Result<i32> {
    match args.cmd {
        FsbenchCmd::Run(opts) => cmd_run(opts, p, kind, cfg).await,
        FsbenchCmd::Compare {
            a,
            b,
            force,
            threshold_scale,
        } => cmd_compare(&a, &b, force, threshold_scale),
        FsbenchCmd::ColdReps(opts) => coldreps::run(opts, p, kind, cfg).await,
    }
}

#[allow(clippy::print_stderr)]
fn cmd_compare(a: &std::path::Path, b: &std::path::Path, force: bool, scale: f64) -> Result<i32> {
    let base = result::read(a)?;
    let cur = result::read(b)?;
    let outcome = compare::compare(&base, &cur, force, scale);
    print_compare(&outcome);
    Ok(outcome.exit_code())
}

#[allow(clippy::print_stderr)]
fn print_compare(o: &compare::Outcome) {
    use console::style;
    if !o.refusals.is_empty() {
        let head = if o.forced {
            style("! identity mismatches (forced through):").yellow()
        } else {
            style("✗ compare refused:").red()
        };
        eprintln!("{head}");
        for r in &o.refusals {
            eprintln!("    {r}");
        }
        if !o.forced {
            return;
        }
    }
    for v in &o.verdicts {
        let line = v.to_string();
        if v.verdict.starts_with("regressed") {
            eprintln!("  {}", style(line).red());
        } else if v.verdict.starts_with("improved") {
            eprintln!("  {}", style(line).green());
        } else {
            eprintln!("  {line}");
        }
    }
}

/// One full bench run: submit, sample co-tenancy, parse + validate the
/// PERF stream, assemble the result, and write `<dir>/result.json`.
/// Returns the assembled result so the caller can summarize, compare,
/// or loop over it (cold-reps).
pub(super) async fn run_single(
    p: &dyn Provider,
    kind: ProviderKind,
    cfg: &XtaskConfig,
    dir: &Path,
    seed: &str,
    nonce: &str,
) -> Result<ResultV1> {
    // SIGINT registered before spawning anything — same rationale as
    // stress.rs: default disposition would skip ProcessGuard's killpg
    // and leak the tunnel (I-158).
    let mut sigint = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::interrupt())?;

    let mut submission = submit::submit(p, cfg, dir, seed, nonce).await?;

    // Co-tenancy watcher. Attribution failing (no rio-cli tunnel, drv
    // never observed) degrades the run to `unattributed` — it does not
    // fail it.
    let client = super::client::client().await?;
    let (stop_tx, stop_rx) = tokio::sync::watch::channel(false);
    let watcher = match CliCtx::open(&client, 0, 0).await {
        Ok(cli) => Some(tokio::spawn(cotenancy::watch(
            client.clone(),
            Arc::new(cli),
            submission.drv_path.clone(),
            stop_rx,
        ))),
        Err(e) => {
            warn!("rio-cli tunnels unavailable ({e:#}); run will be unattributed");
            None
        }
    };

    let status = tokio::select! {
        biased;
        _ = sigint.recv() => anyhow::bail!("interrupted"),
        s = submission.build.child.wait() => s?,
    };
    let _ = stop_tx.send(true);
    let report = match watcher {
        Some(h) => h.await.context("co-tenancy watcher panicked")?,
        None => cotenancy::CotenancyReport {
            attribution: "unattributed".into(),
            ..Default::default()
        },
    };
    drop(submission.build.tunnel);

    let log_text = std::fs::read_to_string(&submission.log_path)
        .with_context(|| format!("read build log {}", submission.log_path.display()))?;
    if !status.success() {
        let tail: Vec<&str> = log_text.lines().rev().take(25).collect();
        let tail: Vec<&str> = tail.into_iter().rev().collect();
        anyhow::bail!(
            "bench build failed ({status}) — log tail:\n{}\n(full log: {})",
            tail.join("\n"),
            submission.log_path.display()
        );
    }

    let parsed = parse::parse_log(&log_text);
    parse::validate(&parsed)?;
    anyhow::ensure!(
        parsed.meta.get("seed").map(String::as_str) == Some(seed),
        "echoed seed {:?} != submitted seed {seed} — a stale eval-cache drv was built",
        parsed.meta.get("seed")
    );
    let workload_version: u32 = parsed
        .meta
        .get("workload_version")
        .and_then(|v| v.parse().ok())
        .context("PERF meta lacks workload_version")?;
    anyhow::ensure!(
        workload_version == 1,
        "workload_version {workload_version} unknown to this xtask — update the workload handling"
    );

    let assembled = assemble(&parsed, &report, seed, nonce, kind, workload_version)?;
    result::write(&assembled, &dir.join("result.json"))?;
    Ok(assembled)
}

#[allow(clippy::print_stderr)]
async fn cmd_run(
    opts: RunOpts,
    p: &dyn Provider,
    kind: ProviderKind,
    cfg: &XtaskConfig,
) -> Result<i32> {
    let seed = opts.seed.clone();
    // Per-run nonce = run id. It keys ONLY the bench-run drv: with the
    // fixed default seed both fsbench drvs would otherwise hash
    // identically run-over-run, the previous run's output would
    // already be valid remotely, and nix would skip executing the
    // benchmark entirely. Alphanumeric so it can live in store-path
    // names.
    let nonce: String = rand::rng()
        .sample_iter(&Alphanumeric)
        .take(12)
        .map(|c| char::from(c).to_ascii_lowercase())
        .collect();

    let ts = jiff::Timestamp::now().as_second();
    let dir = repo_root().join(".fsbench").join(ts.to_string());
    std::fs::create_dir_all(&dir)?;
    info!("run dir: {} (seed {seed}, nonce {nonce})", dir.display());

    let mut assembled = run_single(p, kind, cfg, &dir, &seed, &nonce).await?;
    let path = dir.join("result.json");
    eprintln!();
    print_summary(&assembled, &path);

    // Auto-compare against the named baseline (criterion-style:
    // absence is informational, not an error).
    let mut exit = 0;
    if !opts.no_compare {
        match baseline::load(&opts.baseline)? {
            None => info!(
                "no baseline '{}' — skipping compare (save one with --save-baseline {})",
                opts.baseline, opts.baseline
            ),
            Some(base) => {
                eprintln!();
                eprintln!("vs baseline '{}':", opts.baseline);
                let outcome = compare::compare(&base, &assembled, opts.force, opts.threshold_scale);
                print_compare(&outcome);
                // Persist the verdicts into the result file so the
                // artifact is self-describing.
                assembled.compare = Some(result::CompareBlock {
                    baseline: opts.baseline.clone(),
                    verdicts: outcome.verdicts.iter().map(ToString::to_string).collect(),
                });
                result::write(&assembled, &path)?;
                exit = outcome.exit_code();
            }
        }
    }

    // Save AFTER comparing — the run is judged against the OLD
    // baseline before it can become the new one.
    if let Some(name) = &opts.save_baseline {
        match baseline::save(&assembled, name) {
            Ok(p) => info!("baseline '{name}' saved → {}", p.display()),
            Err(baseline::SaveError::Refused(msg)) => {
                eprintln!("{} {msg}", console::style("✗").red());
                exit = refusal_exit(exit);
            }
            Err(baseline::SaveError::Io(e)) => return Err(e),
        }
    }
    Ok(exit)
}

/// A save refusal exits 2, same as a compare refusal — but never
/// downgrades a regression verdict (3) already in hand.
fn refusal_exit(current: i32) -> i32 {
    current.max(compare::EXIT_REFUSED)
}

fn meta_str(parsed: &parse::ParsedRun, key: &str) -> Result<String> {
    parsed
        .meta
        .get(key)
        .cloned()
        .with_context(|| format!("PERF meta lacks {key}"))
}

fn meta_u64(parsed: &parse::ParsedRun, key: &str) -> Result<u64> {
    meta_str(parsed, key)?
        .parse()
        .with_context(|| format!("PERF meta {key} is not a u64"))
}

fn assemble(
    parsed: &parse::ParsedRun,
    report: &cotenancy::CotenancyReport,
    seed: &str,
    run_id: &str,
    kind: ProviderKind,
    workload_version: u32,
) -> Result<ResultV1> {
    let repo = crate::git::open()?;
    let image_tag = crate::git::image_tag(&repo)?;
    let phases = result::phases_from_run(parsed);
    let cluster_metrics = result::cluster_metrics(parsed, report);
    Ok(ResultV1 {
        schema: result::SCHEMA.into(),
        run_id: run_id.into(),
        seed: seed.into(),
        created_at: jiff::Timestamp::now().to_string(),
        git: result::GitInfo {
            commit: crate::git::short_sha(&repo)?,
            dirty: image_tag.contains("-dirty-"),
            image_tag,
        },
        cluster: result::ClusterInfo {
            provider: kind.to_string(),
            context: super::client::current_context().unwrap_or_else(|_| "unknown".into()),
        },
        placement: result::Placement {
            node: report.node.clone(),
            instance_type: report.instance_type.clone(),
            capacity_type: report.capacity_type.clone(),
            // The sandbox's uname -r IS the node kernel; the node
            // object's version is recorded by the watcher for
            // cross-checking but the bench-observed one is canonical.
            kernel: parsed
                .meta
                .get("kernel")
                .cloned()
                .unwrap_or_else(|| "unknown".into()),
            ami: report.ami.clone(),
            attribution: report.attribution.clone(),
            contended: report.contended,
            max_co_tenants: report.max_co_tenants,
            cotenancy_samples: report.samples.len() as u64,
        },
        workload: result::Workload {
            version: workload_version,
            // Identity + honesty references, echoed from the PERF meta
            // line (the manifest is the source of truth; the bench
            // binary stamps it through).
            dataset_digest: meta_str(parsed, "dataset_digest")?,
            files: meta_u64(parsed, "files")?,
            total_bytes: meta_u64(parsed, "dataset_bytes")?,
            unique_chunk_bytes: meta_u64(parsed, "unique_chunk_bytes")?,
            unique_chunk_bytes_storm: meta_u64(parsed, "unique_chunk_bytes_storm")?,
            jq_src: parsed.meta.get("jq_src").cloned(),
            toolchain: parsed.meta.get("toolchain").cloned(),
            closure: "python3".into(),
            phases: phases.keys().cloned().collect(),
            reps: 3,
        },
        phases,
        cluster_metrics,
        compare: None,
    })
}

#[allow(clippy::print_stderr)]
fn print_summary(r: &ResultV1, path: &std::path::Path) {
    use console::style;
    let metric =
        |phase: &str, key: &str| -> Option<f64> { r.phases.get(phase)?.metrics.get(key)?.value };
    let p99 =
        |phase: &str, key: &str| -> Option<f64> { r.phases.get(phase)?.metrics.get(key)?.p99 };
    eprintln!(
        "{} fsbench {} on {} ({} {}, kernel {})",
        style("▸").blue(),
        r.seed,
        r.placement.node.as_deref().unwrap_or("<unattributed>"),
        r.placement.instance_type.as_deref().unwrap_or("?"),
        r.placement.capacity_type.as_deref().unwrap_or("?"),
        r.placement.kernel,
    );
    let fmt = |v: Option<f64>| v.map_or("-".into(), |v| format!("{v:.1}"));
    eprintln!(
        "  read_storm  cold {} MiB/s  warm {} MiB/s  local-warm {} MiB/s",
        fmt(metric("read_storm_cold", "mib_s")),
        fmt(metric("read_storm_warm", "mib_s")),
        fmt(metric("read_storm_local_warm", "mib_s")),
    );
    eprintln!(
        "  randread    cold p99 {} µs  warm {} IOPS  local-warm {} IOPS",
        fmt(p99("randread_cold", "io_ns").map(|v| v / 1000.0)),
        fmt(metric("randread_warm", "iops")),
        fmt(metric("randread_local_warm", "iops")),
    );
    eprintln!(
        "  slowdown    warm-read {}×  randread {}×",
        fmt(metric("ratios", "warm_read_slowdown")),
        fmt(metric("ratios", "randread_warm_slowdown")),
    );
    match (
        r.placement.attribution.as_str(),
        r.cluster_metrics.honest_cold,
    ) {
        ("exact", Some(true)) => {
            let tag = if r.placement.contended {
                format!(
                    "CONTENDED (max {} co-tenants) — not baseline-eligible",
                    r.placement.max_co_tenants
                )
            } else {
                "exclusive".into()
            };
            eprintln!("  placement   exact, {tag}; cold-honesty PASSED");
            if let Some(note) = &r.cluster_metrics.honesty_note {
                eprintln!("  note        {note}");
            }
        }
        ("exact", Some(false)) => eprintln!(
            "  {} cold-honesty FAILED (dishonest-cold) — cold numbers untrustworthy, \
             baseline save refused",
            style("✗").red()
        ),
        ("exact", None) => {
            eprintln!("  placement   exact; cold-honesty not computable (metric deltas invalid)");
        }
        _ => eprintln!(
            "  {} unattributed — no co-tenancy/honesty data; not baseline-eligible",
            style("·").dim()
        ),
    }
    eprintln!("  result      {}", path.display());
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn seed_defaults_to_the_fixed_constant_and_is_overridable() {
        #[derive(clap::Parser)]
        struct T {
            #[command(flatten)]
            o: RunOpts,
        }
        // Default: the fixed per-workload seed — this is what makes
        // the dataset drv hash stable across runs (built and uploaded
        // once, cache hit afterwards).
        let t = <T as clap::Parser>::try_parse_from(["t"]).unwrap();
        assert_eq!(t.o.seed, DEFAULT_SEED);
        // Explicit override forces a fresh dataset.
        let t = <T as clap::Parser>::try_parse_from(["t", "--seed", "custom-1"]).unwrap();
        assert_eq!(t.o.seed, "custom-1");
    }

    #[test]
    fn default_seed_is_storepath_safe_and_versioned() {
        // fsbench gen rejects seeds outside [a-zA-Z0-9-] (they land in
        // store-path names), and the seed must carry the workload
        // version so bumping the workload bumps the dataset.
        assert!(
            DEFAULT_SEED
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-')
        );
        assert!(
            DEFAULT_SEED.ends_with("w1"),
            "DEFAULT_SEED must encode the workload version"
        );
    }

    #[test]
    fn save_refusal_maps_to_exit_2_without_downgrading_regression() {
        // The documented vocabulary: 0 ok / 2 refusal / 3 regressed.
        // A refused save on an otherwise-clean run exits 2 (it IS a
        // refusal — exiting 1 made it indistinguishable from an
        // infrastructure error); a regression verdict already in hand
        // is the more important signal and must not be downgraded.
        assert_eq!(refusal_exit(0), compare::EXIT_REFUSED);
        assert_eq!(refusal_exit(compare::EXIT_REFUSED), compare::EXIT_REFUSED);
        assert_eq!(
            refusal_exit(compare::EXIT_REGRESSED),
            compare::EXIT_REGRESSED
        );
    }
}
