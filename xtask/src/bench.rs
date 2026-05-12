//! Benchmark fixture capture for `actor_pg_bench`.
//!
//! `xtask bench capture --repo R --repo-root D --targets-file F
//! --nix-eval-jobs B --out-dag dag.json --out-traffic traffic.json`
//!
//! Fetches merged-PR timestamps and merge commits from GitHub,
//! evaluates the targets file at each commit in a temporary worktree,
//! and emits two fixtures:
//!
//!   * `traffic.json`: `[{gap_secs, services}]` from the
//!     adjacent-commit drv-hash diff. Ground truth for what CI
//!     rebuilt; no file-path heuristics.
//!   * `dag.json`: the derivation closure of the most recent commit's
//!     targets, with each node marked cached/to-build via parallel
//!     `nix-store --dry-run` (the same substitutability predicate the
//!     gateway uses at submit time).
//!
//! Both fixtures carry no source-identifying data (no PR numbers,
//! authors, file paths, repo names, or timestamps). See
//! `actor_pg_bench` for usage.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail};
use clap::{Args, Subcommand};
use futures_util::stream::{self, StreamExt as _};
use serde::Serialize;

/// Display-only stderr decode (P0020/P0290 forbids lossy on PARSE
/// paths; these are diagnostics shown to a human).
#[allow(clippy::disallowed_methods)]
fn lossy(b: &[u8]) -> std::borrow::Cow<'_, str> {
    String::from_utf8_lossy(b)
}

#[derive(Args)]
pub struct BenchArgs {
    #[command(subcommand)]
    cmd: BenchCmd,
}

#[derive(Subcommand)]
enum BenchCmd {
    /// Capture DAG and traffic fixtures from a target repo's PR history.
    Capture(CaptureArgs),
}

pub async fn run(args: BenchArgs) -> Result<()> {
    match args.cmd {
        BenchCmd::Capture(a) => capture(a).await,
    }
}

fn num_cpus() -> usize {
    std::thread::available_parallelism().map_or(4, |n| n.get())
}

fn full_path(key: &str) -> String {
    if key.starts_with("/nix/store/") {
        key.to_string()
    } else {
        format!("/nix/store/{key}")
    }
}

fn drv_hash(path: &str) -> Result<String> {
    let base = path
        .strip_prefix("/nix/store/")
        .context("not a store path")?;
    let h = &base[..base.find('-').context("no `-` in store path basename")?];
    if h.len() != 32 {
        bail!("malformed store hash in {path}");
    }
    Ok(h.to_string())
}

/// `<hash>-<pname>-<version>.drv` → `pname`.
fn pname(path: &str) -> String {
    let stem = path
        .rsplit('/')
        .next()
        .unwrap_or(path)
        .trim_end_matches(".drv");
    let after_hash = stem.split_once('-').map_or(stem, |(_, r)| r);
    match after_hash.rsplit_once('-') {
        Some((n, v)) if v.chars().next().is_some_and(|c| c.is_ascii_digit()) => n.to_string(),
        _ => after_hash.to_string(),
    }
}

// ─── Wire format ─────────────────────────────────────────────────────

/// One derivation. Matches `actor_pg_bench`'s `FixtureNode`.
#[derive(Serialize)]
struct Node {
    drv_hash: String,
    drv_path: String,
    pname: String,
    system: String,
    output_names: Vec<String>,
    is_fixed_output: bool,
    required_features: Vec<String>,
    /// `nix-store --dry-run` "will be fetched" → cut by the bench.
    cached: bool,
}

#[derive(Serialize)]
struct DagFixture {
    /// Service (Nix attr) name → root drv_hash.
    roots: BTreeMap<String, String>,
    nodes: Vec<Node>,
    /// `[parent_hash, child_hash]`; parent depends on child.
    edges: Vec<(String, String)>,
}

/// Carries ONLY arrival timing and abstract service labels. Safe to
/// commit.
#[derive(Serialize)]
struct TrafficFixture {
    events: Vec<TrafficEvent>,
}

#[derive(Serialize)]
struct TrafficEvent {
    gap_secs: f64,
    /// Attrs whose drv hash changed at this merge commit.
    services: Vec<String>,
}

// ─── capture ─────────────────────────────────────────────────────────

#[derive(Args)]
struct CaptureArgs {
    /// GitHub `org/repo`. Not stored in either output.
    #[arg(long)]
    repo: String,
    /// Base branch to filter merged PRs by (CI runs on merges to
    /// this branch). Without it, PRs merged into feature branches
    /// get counted as builds.
    #[arg(long, default_value = "main")]
    base: String,
    /// Local checkout containing the merge commits (`git fetch` first).
    #[arg(long)]
    repo_root: PathBuf,
    /// Repo-relative Nix file evaluating to the CI build-target attrset.
    #[arg(long)]
    targets_file: PathBuf,
    /// Path to `nix-eval-jobs` (build it from the target repo's
    /// tooling so its plugin builtins match).
    #[arg(long)]
    nix_eval_jobs: PathBuf,
    /// Output path for the DAG fixture.
    #[arg(long, default_value = "dag.bench-dag.json")]
    out_dag: PathBuf,
    /// Output path for the traffic fixture.
    #[arg(long, default_value = "traffic.bench-traffic.json")]
    out_traffic: PathBuf,
    /// Merged PRs to fetch.
    #[arg(long, default_value_t = 100)]
    limit: usize,
    /// Concurrent per-commit evaluations and dry-runs.
    #[arg(short, long, default_value_t = num_cpus().min(8))]
    jobs: usize,
    /// Workers per `nix-eval-jobs` invocation.
    #[arg(long, default_value_t = 4)]
    eval_workers: usize,
}

async fn capture(args: CaptureArgs) -> Result<()> {
    // ── 1. Merged PRs: timestamps + merge SHAs.
    eprintln!("fetching {} merged PRs from {}...", args.limit, args.repo);
    let out = Command::new("gh")
        .args([
            "pr",
            "list",
            "--repo",
            &args.repo,
            "--state",
            "merged",
            "--base",
            &args.base,
            "--limit",
            &args.limit.to_string(),
            "--json",
            "mergedAt,mergeCommit",
        ])
        .output()
        .context("gh pr list")?;
    if !out.status.success() {
        bail!("gh pr list: {}", lossy(&out.stderr));
    }
    #[derive(serde::Deserialize)]
    struct Pr {
        #[serde(rename = "mergedAt")]
        merged_at: jiff::Timestamp,
        #[serde(rename = "mergeCommit")]
        merge_commit: Option<serde_json::Value>,
    }
    let mut prs: Vec<Pr> = serde_json::from_slice(&out.stdout)?;
    prs.retain(|p| p.merge_commit.is_some());
    prs.sort_by_key(|p| p.merged_at);
    let mut commits: Vec<(jiff::Timestamp, String)> = prs
        .into_iter()
        .filter_map(|p| {
            Some((
                p.merged_at,
                p.merge_commit?.get("oid")?.as_str()?.to_string(),
            ))
        })
        .collect();
    if commits.is_empty() {
        bail!("no merged PRs found");
    }

    // Fetch the base branch so the merge commits are present locally.
    // Trailing commits may STILL be missing — PRs merge faster than a
    // `gh pr list` + `git fetch` round-trip — so drop those rather
    // than bailing.
    eprintln!("git fetch origin {}...", args.base);
    let fetch = Command::new("git")
        .arg("-C")
        .arg(&args.repo_root)
        .args(["fetch", "origin", &args.base])
        .output()
        .context("git fetch")?;
    if !fetch.status.success() {
        bail!("git fetch: {}", lossy(&fetch.stderr));
    }
    while let Some((_, sha)) = commits.last() {
        if git_has_commit(&args.repo_root, sha)? {
            break;
        }
        eprintln!("skip {sha} (not fetched yet)");
        commits.pop();
    }
    if commits.is_empty() {
        bail!("no merged PRs reachable in {}", args.repo_root.display());
    }

    // ── 2. Per-commit eval: attr → drv_path.
    eprintln!(
        "{} PRs; evaluating each commit ({} concurrent)...",
        commits.len(),
        args.jobs
    );
    let mut maps: Vec<Option<BTreeMap<String, String>>> = vec![None; commits.len()];
    let results: Vec<(usize, Result<BTreeMap<String, String>>)> =
        stream::iter(commits.iter().enumerate().map(|(i, (_, sha))| {
            let repo = args.repo_root.clone();
            let target = args.targets_file.clone();
            let nej = args.nix_eval_jobs.clone();
            let workers = args.eval_workers;
            let sha = sha.clone();
            async move {
                let r = tokio::task::spawn_blocking(move || {
                    eval_at(&repo, &sha, &target, &nej, workers)
                })
                .await
                .context("task panicked")
                .and_then(|r| r);
                (i, r)
            }
        }))
        .buffer_unordered(args.jobs)
        .collect()
        .await;
    for (i, r) in results {
        match r {
            Ok(m) => maps[i] = Some(m),
            // Tolerate: a transiently broken commit is a no-build PR.
            Err(e) => eprintln!("warn: eval failed at commit {i}: {e:#}"),
        }
    }

    // ── 3. Traffic: adjacent-commit drv-hash diff.
    let mut events = Vec::new();
    let mut prev_t: Option<jiff::Timestamp> = None;
    let mut prev_m: Option<&BTreeMap<String, String>> = None;
    for ((t, _), m) in commits.iter().zip(&maps) {
        let gap_secs = prev_t.map_or(0.0, |p| {
            (*t - p).total(jiff::Unit::Second).unwrap_or(0.0).max(0.0)
        });
        prev_t = Some(*t);
        let Some(m) = m else { continue };
        if let Some(pm) = prev_m {
            let changed: Vec<String> = m
                .iter()
                .filter(|(k, v)| pm.get(*k).map(|p| drv_hash(p).ok()) != Some(drv_hash(v).ok()))
                .map(|(k, _)| k.clone())
                .collect();
            if !changed.is_empty() {
                events.push(TrafficEvent {
                    gap_secs,
                    services: changed,
                });
            }
        }
        prev_m = Some(m);
    }
    let span: f64 = events.iter().map(|e| e.gap_secs).sum();
    eprintln!(
        "{}/{} commits triggered a rebuild ({:.1}h, {:.1} builds/h)",
        events.len(),
        commits.len(),
        span / 3600.0,
        events.len() as f64 * 3600.0 / span.max(1.0),
    );

    // ── 4. DAG: closure of the LATEST commit's targets, with cached
    // flags from a parallel substitutability probe. Roots restricted
    // to attrs that show up in the traffic so the bench's BFS has
    // somewhere to go for every event.
    let latest = maps
        .iter()
        .rev()
        .find_map(|m| m.as_ref())
        .context("no commit evaluated successfully")?;
    let active: BTreeSet<&str> = events
        .iter()
        .flat_map(|e| e.services.iter().map(String::as_str))
        .collect();
    let roots: BTreeMap<String, String> = latest
        .iter()
        .filter(|(k, _)| active.contains(k.as_str()))
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect();
    if roots.is_empty() {
        bail!("no traffic event matches an evaluable target — widen --limit");
    }
    eprintln!("extracting DAG for {} active services...", roots.len());
    let dag = extract_dag(&roots, args.jobs).await?;

    let n_events = events.len();
    serde_json::to_writer(
        std::fs::File::create(&args.out_traffic)?,
        &TrafficFixture { events },
    )?;
    serde_json::to_writer(std::fs::File::create(&args.out_dag)?, &dag)?;
    eprintln!(
        "wrote {} ({} nodes) and {} ({n_events} events)",
        args.out_dag.display(),
        dag.nodes.len(),
        args.out_traffic.display(),
    );
    Ok(())
}

// ─── DAG extraction ──────────────────────────────────────────────────

async fn extract_dag(roots: &BTreeMap<String, String>, jobs: usize) -> Result<DagFixture> {
    let drv_paths: Vec<&String> = roots.values().collect();
    let out = Command::new("nix")
        .args(["derivation", "show", "-r"])
        .args(&drv_paths)
        .output()
        .context("nix derivation show -r")?;
    if !out.status.success() {
        bail!("nix derivation show: {}", lossy(&out.stderr));
    }
    let raw: serde_json::Value = serde_json::from_slice(&out.stdout)?;
    // v4 (Nix 2.34+) wraps in {"derivations": {...}}; older: bare map.
    let drvs = raw
        .get("derivations")
        .or(Some(&raw))
        .and_then(|v| v.as_object())
        .context("unexpected schema")?
        .clone();

    // Substitutability probe: dry-run each root, union "will be built".
    // Same predicate the gateway uses at submit time.
    eprintln!(
        "probing substitutability ({} root(s), -j{jobs}) ...",
        drv_paths.len()
    );
    let uncached: BTreeSet<String> = stream::iter(drv_paths.iter().map(|d| d.to_string()))
        .map(|d| tokio::task::spawn_blocking(move || will_build(&d)))
        .buffer_unordered(jobs)
        .map(|r| r.context("task panicked").and_then(|r| r))
        .collect::<Vec<_>>()
        .await
        .into_iter()
        .collect::<Result<Vec<_>>>()?
        .into_iter()
        .flatten()
        .collect();

    let mut nodes = Vec::with_capacity(drvs.len());
    let mut edges = Vec::new();
    for (key, d) in &drvs {
        let path = full_path(key);
        let h = drv_hash(&path)?;
        let outputs = d.get("outputs").and_then(|o| o.as_object());
        let mut output_names: Vec<String> = outputs
            .map(|o| o.keys().cloned().collect())
            .unwrap_or_default();
        if output_names.is_empty() {
            output_names.push("out".into());
        }
        output_names.sort();
        nodes.push(Node {
            drv_hash: h.clone(),
            drv_path: path.clone(),
            pname: pname(&path),
            system: d
                .get("system")
                .and_then(|s| s.as_str())
                .unwrap_or("x86_64-linux")
                .into(),
            output_names,
            is_fixed_output: outputs.is_some_and(|o| o.values().any(|v| v.get("hash").is_some())),
            required_features: d
                .pointer("/env/requiredSystemFeatures")
                .and_then(|v| v.as_str())
                .map(|s| s.split_whitespace().map(String::from).collect())
                .unwrap_or_default(),
            cached: !uncached.contains(&path),
        });
        // DerivationEdge semantics: parent depends on child. This drv
        // (the dependent) is the parent, its inputs are children.
        for inp in input_drvs(d) {
            edges.push((h.clone(), drv_hash(&full_path(&inp))?));
        }
    }

    let cached = nodes.iter().filter(|n| n.cached).count();
    let fods = nodes.iter().filter(|n| n.is_fixed_output).count();
    eprintln!(
        "{} roots, {} nodes, {} edges, {fods} FODs, {cached} cached / {} to-build",
        roots.len(),
        nodes.len(),
        edges.len(),
        nodes.len() - cached,
    );
    Ok(DagFixture {
        roots: roots
            .iter()
            .map(|(n, d)| Ok((n.clone(), drv_hash(d)?)))
            .collect::<Result<_>>()?,
        nodes,
        edges,
    })
}

fn input_drvs(d: &serde_json::Value) -> Vec<String> {
    d.get("inputDrvs")
        .or_else(|| d.pointer("/inputs/drvs"))
        .and_then(|v| v.as_object())
        .map(|m| m.keys().cloned().collect())
        .unwrap_or_default()
}

/// `.drv` paths a dry-run would BUILD (vs substitute).
fn will_build(drv: &str) -> Result<BTreeSet<String>> {
    let out = Command::new("nix-store")
        .args(["-r", "--dry-run", drv])
        .output()
        .context("nix-store")?;
    let mut set = BTreeSet::new();
    let mut on = false;
    for line in lossy(&out.stderr).lines() {
        if line.contains("will be built:") {
            on = true;
        } else if !line.starts_with("  ") {
            on = false;
        } else if on {
            let p = line.trim();
            if p.starts_with("/nix/store/") && p.ends_with(".drv") {
                set.insert(p.to_string());
            }
        }
    }
    Ok(set)
}

// ─── git + nix-eval-jobs ─────────────────────────────────────────────

fn git_has_commit(repo: &Path, sha: &str) -> Result<bool> {
    Ok(Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(["cat-file", "-e", &format!("{sha}^{{commit}}")])
        .status()
        .context("git cat-file")?
        .success())
}

/// `nix-eval-jobs <targets_file>` at `sha` in an ephemeral worktree.
/// Returns `{attr: drv_path}`. Per-attr errors are tolerated (skipped);
/// real CI manifests occasionally carry a transiently-broken attr.
fn eval_at(
    repo: &Path,
    sha: &str,
    targets_file: &Path,
    nej: &Path,
    workers: usize,
) -> Result<BTreeMap<String, String>> {
    // Worktree alongside the repo, NOT in $TMPDIR — the source tree
    // can be tens of GiB and $TMPDIR is often tmpfs.
    let parent = repo.parent().unwrap_or(repo);
    let tmp = tempfile::Builder::new()
        .prefix(".rio-bench-")
        .tempdir_in(parent)
        .context("mktemp")?;
    // git refuses to add a worktree at an existing dir; use a child.
    let wt = tmp.path().join("co");
    let _guard = scopeguard::guard((), |()| {
        // Best-effort: remove the worktree admin record; the dir goes
        // with TempDir drop. `git worktree prune` sweeps strays.
        let _ = Command::new("git")
            .arg("-C")
            .arg(repo)
            .args(["worktree", "remove", "--force"])
            .arg(&wt)
            .output();
    });
    let add = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(["worktree", "add", "--detach"])
        .arg(&wt)
        .arg(sha)
        .output()
        .context("git worktree add")?;
    if !add.status.success() {
        bail!("git worktree add: {}", lossy(&add.stderr));
    }

    // GC-roots dir is required (nix-eval-jobs writes a symlink per
    // drv); keep it temp-scoped so .drv links don't accumulate.
    let gc = tmp.path().join("gcroots");
    std::fs::create_dir(&gc)?;
    let out = Command::new(nej)
        .arg("--workers")
        .arg(workers.to_string())
        .arg("--gc-roots-dir")
        .arg(&gc)
        .arg("--force-recurse")
        .arg(wt.join(targets_file))
        .output()
        .context("nix-eval-jobs")?;
    if !out.status.success() {
        bail!("nix-eval-jobs: {}", lossy(&out.stderr));
    }
    #[derive(serde::Deserialize)]
    struct Line {
        attr: String,
        #[serde(rename = "drvPath")]
        drv_path: Option<String>,
        error: Option<String>,
    }
    let mut map = BTreeMap::new();
    for line in std::str::from_utf8(&out.stdout)
        .context("nej output not utf8")?
        .lines()
    {
        let l: Line = serde_json::from_str(line).context("parse nej line")?;
        if l.error.is_none()
            && let Some(p) = l.drv_path
        {
            map.insert(l.attr, p);
        }
    }
    Ok(map)
}
