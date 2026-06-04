//! `fsbench cold-reps` — N cold reps of the full bench with a cache
//! eviction between each, aggregated into mean/stddev/stderr form.
//!
//! Before/after-agnostic: it measures whatever image is deployed and
//! emits ONE `cold-reps.json`. The comparison is an operator matter of
//! which commit was deployed for each result file (see `graph.py`).
//!
//! Per-rep honesty is the safety net: a rep that lands on a warm /
//! non-evicted node fails `honest_cold`, or lands on a different node
//! than the run's first rep, is dropped and redone (bounded by
//! `--max-redos`). All-reps-dropped is a loud failure, not a silent
//! wrong answer.

use std::collections::BTreeMap;

use anyhow::Result;
use rand::{RngExt, distr::Alphanumeric};
use tracing::{info, warn};

use super::ColdRepsOpts;
use super::evict;
use super::result::{self, AggStats, ColdRepRow, ColdRepsResult, ResultV1};
use crate::config::XtaskConfig;
use crate::k8s::provider::{Provider, ProviderKind};
use crate::sh::repo_root;

fn phase_value(res: &ResultV1, phase: &str, key: &str) -> Option<f64> {
    res.phases.get(phase)?.metrics.get(key)?.value
}

fn phase_p99(res: &ResultV1, phase: &str, key: &str) -> Option<f64> {
    res.phases.get(phase)?.metrics.get(key)?.p99
}

pub(super) async fn run(
    opts: ColdRepsOpts,
    p: &dyn Provider,
    kind: ProviderKind,
    cfg: &XtaskConfig,
) -> Result<i32> {
    let ts = jiff::Timestamp::now().as_second();
    let dir = repo_root().join(".fsbench").join(ts.to_string());
    std::fs::create_dir_all(&dir)?;
    info!(
        "cold-reps dir: {} (reps {}, seed {})",
        dir.display(),
        opts.reps,
        opts.seed
    );

    let client = crate::k8s::client::client().await?;

    // Cordon bookkeeping lives OUTSIDE run_inner so the uncordon
    // cleanup runs on every exit path (success, gate exhaustion, and
    // any `?` error inside the rep loop).
    let mut cordoned: Vec<String> = Vec::new();
    let mut protected: Option<String> = None;
    let out = run_inner(
        &opts,
        p,
        kind,
        cfg,
        &dir,
        &client,
        &mut cordoned,
        &mut protected,
    )
    .await;
    if let Some(node) = &protected {
        evict::remove_do_not_disrupt(&client, node).await;
    }
    evict::uncordon_nodes(&client, &cordoned).await;
    out
}

#[allow(clippy::too_many_arguments)]
async fn run_inner(
    opts: &ColdRepsOpts,
    p: &dyn Provider,
    kind: ProviderKind,
    cfg: &XtaskConfig,
    dir: &std::path::Path,
    client: &kube::Client,
    cordoned: &mut Vec<String>,
    protected: &mut Option<String>,
) -> Result<i32> {
    let mut accepted: Vec<ColdRepRow> = Vec::new();
    let mut first_result: Option<ResultV1> = None;
    let mut target_node: Option<String> = None;
    let mut redos: u32 = 0;

    while accepted.len() < opts.reps as usize && redos <= opts.max_redos {
        // Once the first rep has pinned the target node, cordon every
        // OTHER builder node before each rep — spot churn provisions
        // replacements mid-run and reps that land there are wasted
        // redos. The node-pin and honesty gates below stay as they
        // are; the cordon only stops the placement coin-flip.
        if let Some(target) = &target_node {
            evict::cordon_other_builders(client, target, cordoned).await?;
        }

        // Cold the builder node(s) before every rep — this is what
        // makes the next run's cold phase actually cold.
        evict::evict_builder_caches(client).await?;

        // Fresh per-run nonce so the bench-run drv re-executes despite
        // the fixed dataset seed (same rationale as cmd_run).
        let nonce: String = rand::rng()
            .sample_iter(&Alphanumeric)
            .take(12)
            .map(|c| char::from(c).to_ascii_lowercase())
            .collect();

        let repdir = dir.join(format!("rep-{}", accepted.len() + 1 + redos as usize));
        std::fs::create_dir_all(&repdir)?;

        let res = match super::run_single(p, kind, cfg, &repdir, &opts.seed, &nonce).await {
            Ok(r) => r,
            Err(e) => {
                warn!("rep build failed (dropped): {e:#}");
                redos += 1;
                continue;
            }
        };

        let node = res.placement.node.clone();
        let honest = res.cluster_metrics.honest_cold == Some(true);
        if target_node.is_none() {
            target_node = node.clone();
            // Pin survival: consolidation reaps the target between
            // reps (evict-cache leaves it looking idle) and the run
            // can never complete against a dead node. Removed in
            // `run()` on every exit path.
            if let Some(t) = &target_node {
                evict::annotate_do_not_disrupt(client, t).await?;
                *protected = Some(t.clone());
            }
        }
        if node != target_node || !honest {
            warn!(
                "rep dropped: node={:?} (target {:?}), honest_cold={:?}",
                node, target_node, res.cluster_metrics.honest_cold
            );
            redos += 1;
            continue;
        }

        let promote = res
            .cluster_metrics
            .mountd
            .as_ref()
            .and_then(|m| m.promote_bytes_cold_window_delta);
        accepted.push(ColdRepRow {
            nonce,
            node,
            honest_cold: res.cluster_metrics.honest_cold,
            jq_total_wall_ms: phase_value(&res, "jq_build_cold", "total_wall_ms"),
            jq_configure_wall_ms: phase_value(&res, "jq_build_cold", "configure_wall_ms"),
            jq_make_wall_ms: phase_value(&res, "jq_build_cold", "make_wall_ms"),
            read_storm_cold_mib_s: phase_value(&res, "read_storm_cold", "mib_s"),
            read_storm_cold_open_p99_ns: phase_p99(&res, "read_storm_cold", "open_ns"),
            promote_bytes_cold: promote,
        });
        if first_result.is_none() {
            first_result = Some(res);
        }
    }

    if accepted.len() < opts.reps as usize {
        warn!(
            "accepted {} of {} requested cold reps — max_redos {} exhausted; emitting what we have",
            accepted.len(),
            opts.reps,
            opts.max_redos
        );
    }

    let Some(first) = first_result else {
        warn!("zero cold reps accepted — nothing measured");
        return Ok(2);
    };

    // Aggregate the cold metrics across the accepted reps.
    let mut metrics: BTreeMap<String, AggStats> = BTreeMap::new();
    let mut agg = |key: &str, unit: &str, get: fn(&ColdRepRow) -> Option<f64>| {
        let xs: Vec<f64> = accepted.iter().filter_map(get).collect();
        if let Some(stats) = AggStats::from_samples(unit, &xs) {
            metrics.insert(key.to_string(), stats);
        }
    };
    agg("jq_build_cold.total_wall_ms", "ms", |r| r.jq_total_wall_ms);
    agg("jq_build_cold.configure_wall_ms", "ms", |r| {
        r.jq_configure_wall_ms
    });
    agg("jq_build_cold.make_wall_ms", "ms", |r| r.jq_make_wall_ms);
    agg("read_storm_cold.mib_s", "mib_s", |r| {
        r.read_storm_cold_mib_s
    });

    let reps_accepted = accepted.len() as u32;
    let cr = ColdRepsResult {
        schema: result::COLD_REPS_SCHEMA.into(),
        created_at: jiff::Timestamp::now().to_string(),
        git: first.git.clone(),
        cluster: first.cluster.clone(),
        node: first.placement.node.clone(),
        instance_type: first.placement.instance_type.clone(),
        kernel: first.placement.kernel.clone(),
        workload: first.workload.clone(),
        reps_requested: opts.reps,
        reps_accepted,
        reps_dropped: redos,
        metrics,
        per_rep: accepted,
    };

    result::write_cold_reps(&cr, &dir.join("cold-reps.json"))?;
    if let Some(save) = &opts.save {
        result::write_cold_reps(&cr, save)?;
    }
    print_summary(&cr);
    Ok(0)
}

#[allow(clippy::print_stderr)]
fn print_summary(cr: &ColdRepsResult) {
    eprintln!(
        "cold-reps on {} ({}): {} accepted, {} dropped",
        cr.node.as_deref().unwrap_or("<unattributed>"),
        cr.instance_type.as_deref().unwrap_or("?"),
        cr.reps_accepted,
        cr.reps_dropped,
    );
    for (key, s) in &cr.metrics {
        eprintln!(
            "  {key}: {:.1} ± {:.1} {} ({:.0}% spread, n={})",
            s.mean,
            s.stderr,
            s.unit,
            s.rel_spread * 100.0,
            s.n
        );
    }
}
