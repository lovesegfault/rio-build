//! Bench submission: local prebuild of the fsbench bin, an explicit
//! build-time closure copy to the gateway store, the I-161 pre-eval
//! warm step (which also yields the drv path for attribution), a
//! dry-run plan sanity gate, then one `nix build --store ssh-ng://…`
//! through the shared tunnel path. Strictly serial — exactly one build
//! per run, by design (parallel benches generate the contention they
//! then measure).
//!
//! Why the WHOLE build-time closure and not just the bin's runtime
//! closure: the gateway submits the full inputDrvs graph unpruned
//! (rio-gateway translate.rs `reconstruct_dag`), and the scheduler's
//! cache-hit conversion (merge.rs `apply_cached_hits`) honors a
//! closure invariant — a node whose output is valid converts to
//! Completed only once ALL of its inputDrvs are Completed. A mid-graph
//! valid output therefore never prunes its ancestors. Shipping every
//! ancestor output makes the leaves (FOD tarballs, no inputDrvs)
//! convert immediately and the fixed point walk the graph bottom-up,
//! leaving exactly fsbench-dataset + fsbench-run to execute on the
//! builder pool.

use std::path::{Path, PathBuf};
use std::process::Stdio;

use anyhow::{Context, Result, bail, ensure};
use tracing::info;

use crate::config::XtaskConfig;
use crate::k8s::provider::Provider;
use crate::k8s::shared::{self, RemoteBuild};
use crate::sh::repo_root;

pub struct Submission {
    /// The fsbench-run drv path from pre-eval — the co-tenancy join
    /// key. Always `Some` since the closure copy is computed from it
    /// (submission hard-fails without); the Option shape is kept for
    /// the watcher's degraded path.
    pub drv_path: Option<String>,
    pub log_path: PathBuf,
    pub build: RemoteBuild,
}

pub async fn submit(
    p: &dyn Provider,
    cfg: &XtaskConfig,
    dir: &Path,
    seed: &str,
    nonce: &str,
) -> Result<Submission> {
    // The local prebuild produces x86_64-linux artifacts; the builder
    // pool consumes them as copied closures. Cross-building the whole
    // rio-builder graph from another host arch would defeat the point
    // of the prebuild.
    ensure!(
        std::env::consts::ARCH == "x86_64" && std::env::consts::OS == "linux",
        "fsbench run must be driven from an x86_64-linux host: the local \
         .#fsbench-bin prebuild is what gets copied to the gateway; \
         on this host it would have to remote-build the whole crate graph"
    );

    // Prebuild: realizes the crate graph locally and fails fast on a
    // broken tree. Its outputs are covered by the closure realize
    // below — no need to capture them here.
    info!("pre-building .#fsbench-bin locally (cached after first run)");
    let status = tokio::process::Command::new("nix")
        .args(["build", ".#fsbench-bin", "--no-link"])
        .current_dir(repo_root())
        .stdin(Stdio::null())
        .status()
        .await
        .context("spawn nix build .#fsbench-bin")?;
    ensure!(status.success(), "nix build .#fsbench-bin failed");

    let installable = format!("{}#fsbench-run", repo_root().display());
    // Seed keys the dataset (fixed by default -> drv reuse across
    // runs); the nonce keys only the run drv so the benchmark
    // re-executes despite the stable dataset.
    let envs = [("FSBENCH_SEED", seed), ("FSBENCH_RUN_NONCE", nonce)];

    // Pre-eval bakes the seed into the drv (local --impure eval; what
    // crosses the gateway is already seeded) and warms the eval cache
    // (I-161). Same envs as the build below — a mismatch would
    // instantiate a DIFFERENT drv and re-open the cold-eval window.
    let drv_path = shared::pre_eval_installable(&installable, &envs);
    let Some(bench_drv) = drv_path.clone() else {
        bail!(
            "pre-eval failed — without the bench drv path the build-time \
             closure cannot be computed, and a closure-less submission \
             schedules the toolchain bootstrap on-cluster; fix the eval \
             error above and re-run"
        );
    };
    info!("bench drv: {bench_drv}");

    // Every ancestor derivation of the bench drv — which includes the
    // dataset drv's ancestors, since the dataset is itself an ancestor
    // of the run drv — excluding the fsbench pair: those two MUST
    // build remotely.
    let requisites = capture_stdout(
        "nix-store",
        &["--query".into(), "--requisites".into(), bench_drv],
    )
    .await?;
    let ancestors = ancestor_drvs(&requisites);
    ensure!(
        !ancestors.is_empty(),
        "bench drv has no ancestor derivations — unexpected eval graph"
    );

    // Realize the ancestors locally: the crate graph is already valid
    // from the prebuild; the nixpkgs slice (python3, stdenv, the rust
    // toolchain) substitutes from cache.nixos.org. The printed output
    // paths are exactly the set the scheduler must see as cache hits.
    info!(
        "realizing {} ancestor derivations locally (first run substitutes \
         the nixpkgs slice; later runs are no-ops)",
        ancestors.len()
    );
    let mut realise_args = vec!["--realise".to_string()];
    realise_args.extend(ancestors.iter().cloned());
    let realized = capture_stdout("nix-store", &realise_args).await?;
    let outputs: Vec<String> = realized
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .map(String::from)
        .collect();
    ensure!(
        !outputs.is_empty(),
        "nix-store --realise printed no output paths"
    );

    // Copy the build-time closure to the gateway, then sanity-check
    // the remote plan, on a dedicated short-lived tunnel. `nix copy`
    // skips remote-valid paths, so only the first run pays the bulk
    // transfer.
    {
        let key = crate::ssh::privkey_path(cfg)?;
        let (port, _tunnel) = p.tunnel(0).await?;
        let store = format!(
            "ssh-ng://rio@localhost:{port}?compress=true&ssh-key={}",
            key.display()
        );

        match nar_size_estimate(&outputs).await {
            Some(bytes) => info!(
                "copying {} ancestor outputs (~{} MiB NAR; remote-valid \
                 paths are skipped) to the gateway store",
                outputs.len(),
                bytes / (1024 * 1024)
            ),
            None => info!(
                "copying {} ancestor outputs to the gateway store \
                 (remote-valid paths are skipped)",
                outputs.len()
            ),
        }
        let mut copy_args: Vec<String> = vec!["copy".into(), "--to".into(), store.clone()];
        copy_args.extend(outputs.iter().cloned());
        let copy = tokio::process::Command::new("nix")
            .args(&copy_args)
            .env("NIX_SSHOPTS", shared::NIX_SSHOPTS_BASE)
            .stdin(Stdio::null())
            .status()
            .await
            .context("spawn nix copy")?;
        ensure!(
            copy.success(),
            "nix copy of the build-time closure to the gateway failed"
        );

        // Fail fast if the remote still plans to build anything beyond
        // the two fsbench drvs — that converts a recurrence of the
        // bootstrap failure class into an immediate local error naming
        // the culprits.
        info!("dry-run: checking the remote build plan");
        let mut cmd = tokio::process::Command::new("nix");
        cmd.args([
            "build",
            "--store",
            &store,
            "--eval-store",
            "auto",
            "--impure",
            "--dry-run",
        ])
        .arg(&installable)
        .env("NIX_SSHOPTS", shared::NIX_SSHOPTS_BASE)
        .stdin(Stdio::null());
        for (k, v) in &envs {
            cmd.env(k, v);
        }
        let plan = cmd.output().await.context("spawn nix build --dry-run")?;
        // Parse path, so no lossy conversion: store paths are ASCII;
        // non-utf8 here means the output is not a plan at all.
        let stderr = std::str::from_utf8(&plan.stderr)
            .context("nix build --dry-run emitted non-utf8 stderr")?;
        ensure!(
            plan.status.success(),
            "nix build --dry-run failed:\n{stderr}"
        );
        let unexpected = unexpected_remote_builds(stderr);
        if !unexpected.is_empty() {
            let shown: Vec<&str> = unexpected.iter().map(String::as_str).take(5).collect();
            bail!(
                "remote plan wants to build {} derivation(s) beyond the fsbench pair \
                 (first {}: {}) — the build-time closure copy did not take; refusing \
                 to schedule a toolchain bootstrap on the cluster",
                unexpected.len(),
                shown.len(),
                shown.join(", ")
            );
        }
        // Tunnel guard drops here; the build below opens its own.
    }

    let log_path = dir.join("build.log");
    let build = shared::spawn_remote_nix_build(p, 0, cfg, &installable, &log_path, &envs).await?;
    Ok(Submission {
        drv_path,
        log_path,
        build,
    })
}

/// Run `program` with `args`, stdout captured, stderr inherited (so
/// substitution and copy progress stays visible to the operator).
async fn capture_stdout(program: &str, args: &[String]) -> Result<String> {
    let out = tokio::process::Command::new(program)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .output()
        .await
        .with_context(|| format!("spawn {program}"))?;
    ensure!(
        out.status.success(),
        "{program} {} failed",
        args.first().map(String::as_str).unwrap_or("")
    );
    String::from_utf8(out.stdout).with_context(|| format!("{program} emitted non-utf8 stdout"))
}

/// Sum of narSize over `paths` via `nix path-info --json`. Best-effort
/// — the copy proceeds without the estimate. Handles both the array
/// (nix ≤2.18) and object-keyed (≥2.19) JSON shapes.
async fn nar_size_estimate(paths: &[String]) -> Option<u64> {
    let mut args: Vec<String> = vec!["path-info".into(), "--json".into()];
    args.extend(paths.iter().cloned());
    let json = capture_stdout("nix", &args).await.ok()?;
    let v: serde_json::Value = serde_json::from_str(&json).ok()?;
    let items: Vec<&serde_json::Value> = match &v {
        serde_json::Value::Array(a) => a.iter().collect(),
        serde_json::Value::Object(o) => o.values().collect(),
        _ => return None,
    };
    Some(
        items
            .iter()
            .filter_map(|i| i.get("narSize").and_then(serde_json::Value::as_u64))
            .sum(),
    )
}

/// `/nix/store/<hash>-<name>[.drv]` → `<name>`.
fn drv_name(path: &str) -> &str {
    let base = path.rsplit('/').next().unwrap_or(path);
    let name = base.split_once('-').map_or(base, |(_, n)| n);
    name.strip_suffix(".drv").unwrap_or(name)
}

/// The two derivations that must execute on the cluster — everything
/// else in the graph is expected to be a remote cache hit.
fn is_fsbench_pair(name: &str) -> bool {
    name.starts_with("fsbench-dataset-") || name.starts_with("fsbench-run-")
}

/// Partition `nix-store --query --requisites <bench-drv>` output into
/// the ancestor derivations to realize and ship. Non-drv lines (input
/// sources) are skipped — sources ride with the submission's
/// derivation closure; what the scheduler's cache-hit conversion needs
/// valid remotely is ancestor OUTPUTS, which realizing these yields.
/// The fsbench pair is excluded: realizing the dataset locally would
/// generate 2.3 GiB on the operator box, and realizing the run drv
/// would execute the benchmark against the local filesystem.
fn ancestor_drvs(requisites: &str) -> Vec<String> {
    requisites
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && l.ends_with(".drv"))
        .filter(|l| !is_fsbench_pair(drv_name(l)))
        .map(String::from)
        .collect()
}

/// Parse `nix build --dry-run` stderr and return the names of
/// planned-to-build derivations that are NOT the fsbench pair.
/// Substituted paths live under "will be fetched" and are fine —
/// only the "will be built" section matters.
fn unexpected_remote_builds(dry_run_stderr: &str) -> Vec<String> {
    let mut in_built = false;
    let mut out = Vec::new();
    for line in dry_run_stderr.lines() {
        if line.contains("will be built") {
            in_built = true;
            continue;
        }
        if !line.starts_with(char::is_whitespace) {
            // Section headers (and any other non-indented chatter) end
            // the built list — "will be fetched" arrives this way.
            in_built = false;
            continue;
        }
        if !in_built {
            continue;
        }
        let name = drv_name(line.trim());
        if !is_fsbench_pair(name) {
            out.push(name.to_string());
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ancestor_drvs_keeps_drvs_drops_pair_and_sources() {
        // Synthetic `nix-store -qR <run-drv>` output: input sources
        // (non-.drv) interleaved with ancestor drvs and the pair.
        let requisites = "\
/nix/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-source
/nix/store/bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb-rustc-1.94.1.drv
/nix/store/cccccccccccccccccccccccccccccccc-rust_rio-builder-0.1.0.drv
/nix/store/dddddddddddddddddddddddddddddddd-python3-3.12.8.drv
/nix/store/eeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee-rio-builder-bin.drv
/nix/store/ffffffffffffffffffffffffffffffff-fsbench-dataset-ab12cd.drv
/nix/store/gggggggggggggggggggggggggggggggg-fsbench-run-ab12cd.drv
/nix/store/hhhhhhhhhhhhhhhhhhhhhhhhhhhhhhhh-default-builder.sh
";
        let got = ancestor_drvs(requisites);
        // Sources skipped (they ride with the drv closure); the pair
        // excluded — those two must execute remotely, and realizing
        // the dataset locally would generate gigabytes here.
        assert_eq!(
            got,
            vec![
                "/nix/store/bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb-rustc-1.94.1.drv",
                "/nix/store/cccccccccccccccccccccccccccccccc-rust_rio-builder-0.1.0.drv",
                "/nix/store/dddddddddddddddddddddddddddddddd-python3-3.12.8.drv",
                "/nix/store/eeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee-rio-builder-bin.drv",
            ]
        );
    }

    #[test]
    fn clean_plan_is_only_the_fsbench_pair() {
        let plan = "\
these 2 derivations will be built:
  /nix/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-fsbench-dataset-ab12cd.drv
  /nix/store/bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb-fsbench-run-ab12cd.drv
these 217 paths will be fetched (412.33 MiB download, 1843.21 MiB unpacked):
  /nix/store/cccccccccccccccccccccccccccccccc-python3-3.12.8
  /nix/store/dddddddddddddddddddddddddddddddd-glibc-2.40-66
";
        assert!(unexpected_remote_builds(plan).is_empty());
    }

    #[test]
    fn toolchain_bootstrap_is_flagged_fetched_paths_are_not() {
        // The observed failure shape: ancestors not valid remotely →
        // the planner schedules the rust toolchain (and our crate
        // graph) for on-cluster builds. Fetched paths must NOT trip
        // the gate.
        let plan = "\
these 3348 derivations will be built:
  /nix/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-fsbench-dataset-ab12cd.drv
  /nix/store/bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb-fsbench-run-ab12cd.drv
  /nix/store/cccccccccccccccccccccccccccccccc-musl-1.2.5.drv
  /nix/store/dddddddddddddddddddddddddddddddd-tinycc-0.9.27.drv
  /nix/store/eeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee-rustc-1.94.1.drv
  /nix/store/ffffffffffffffffffffffffffffffff-rust_rio-builder-0.1.0.drv
these 5 paths will be fetched (1.00 MiB download, 3.00 MiB unpacked):
  /nix/store/gggggggggggggggggggggggggggggggg-bash-5.2p37
";
        let got = unexpected_remote_builds(plan);
        assert_eq!(
            got,
            vec![
                "musl-1.2.5",
                "tinycc-0.9.27",
                "rustc-1.94.1",
                "rust_rio-builder-0.1.0"
            ]
        );
    }

    #[test]
    fn empty_plan_everything_valid_remotely() {
        // Re-running after a completed build: nothing to do at all.
        assert!(unexpected_remote_builds("").is_empty());
    }
}
