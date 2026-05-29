//! `rio-parity eval` — record a v1 replay archive from a Hydra
//! evaluation: reproduce the evaluation locally, gate it on drvPath
//! fidelity, sweep upstream truth, and publish the packed archive.
//!
//! Phases: Hydra structural fetches → source prep (tarball download,
//! unpack, `nix store add-path`) → scoped nix-eval-jobs run → drvPath
//! fidelity gate → closure adjacency + drv members → truth sweep
//! (cache.nixos.org narinfo presence + Hydra buildstatus) → archive
//! staging + mkdwarfs pack → S3 upload with by-recipe idempotency.
//! `--dry-run` stops after the fidelity gate: no closure pass, no truth
//! sweep, no archive, no upload (fidelity.json and a dry-run-marked
//! provenance.json are still written locally, so the run is auditable).

use std::collections::BTreeMap;
use std::path::PathBuf;

use clap::Args;

use crate::evalset::Scope;

/// Parse `--scope`:
///   `full`
///   `constituents:<aggregate-job>`   (e.g. constituents:tested)
///   `jobs:<job1,job2,…>`
///   `jobs-file:<path>`               (one job name per line, `#` comments)
///
/// Explicit job lists (`jobs:`/`jobs-file:`) are sorted and
/// deduplicated here, so listing the same jobs in a different order or
/// with repetitions cannot fork the recipe key digest, issue duplicate
/// per-job Hydra requests, or define the same selection.nix attribute
/// twice (a Nix "attribute already defined" eval failure).
///
/// Aggregate jobs (e.g. `tested`) must be requested via
/// `constituents:<job>`, never listed under `jobs:`/`jobs-file:`:
/// scoped evaluations run nix-eval-jobs without `--constituents`, so a
/// hand-fed aggregate evaluates as a plain job whose drvPath cannot
/// match the constituent-rewritten derivation Hydra stores, and the
/// recording would always come out divergent.
pub fn parse_scope(s: &str) -> anyhow::Result<Scope> {
    if s == "full" {
        return Ok(Scope::Full);
    }
    if let Some(job) = s.strip_prefix("constituents:") {
        anyhow::ensure!(!job.is_empty(), "constituents: needs an aggregate job name");
        return Ok(Scope::Constituents {
            aggregate_job: job.to_string(),
        });
    }
    if let Some(list) = s.strip_prefix("jobs:") {
        let mut jobs: Vec<String> = list
            .split(',')
            .map(str::trim)
            .filter(|j| !j.is_empty())
            .map(String::from)
            .collect();
        anyhow::ensure!(!jobs.is_empty(), "jobs: needs at least one job name");
        jobs.sort();
        jobs.dedup();
        return Ok(Scope::Jobs { jobs });
    }
    if let Some(path) = s.strip_prefix("jobs-file:") {
        let text = std::fs::read_to_string(path)
            .map_err(|e| anyhow::anyhow!("read jobs file {path}: {e}"))?;
        let mut jobs: Vec<String> = text
            .lines()
            .map(str::trim)
            .filter(|l| !l.is_empty() && !l.starts_with('#'))
            .map(String::from)
            .collect();
        anyhow::ensure!(!jobs.is_empty(), "jobs file {path} contains no job names");
        jobs.sort();
        jobs.dedup();
        return Ok(Scope::Jobs { jobs });
    }
    anyhow::bail!(
        "unrecognized --scope {s:?}; expected full, constituents:<job>, jobs:<j1,j2,…>, or jobs-file:<path>"
    )
}

#[derive(Debug, Args)]
pub struct EvalArgs {
    /// Hydra evaluation id (e.g. 1824219).
    #[arg(long)]
    pub hydra_eval: u64,

    /// Evaluation scope: `full` | `constituents:<job>` | `jobs:<j1,j2,…>` | `jobs-file:<path>`.
    ///
    /// Aggregate jobs (e.g. `tested`) must be requested as
    /// `constituents:<job>`, never listed under `jobs:`/`jobs-file:`.
    /// Scoped evaluations run without `--constituents`, so a hand-fed
    /// aggregate evaluates as a plain job whose drvPath can never match
    /// Hydra's rewritten aggregate derivation and the recording would
    /// always be marked divergent.
    #[arg(long)]
    pub scope: String,

    /// `<project>/<jobset>` override; default: derived from the first
    /// build of the eval (one extra Hydra request).
    #[arg(long)]
    pub jobset: Option<String>,

    /// Systems to keep (constituents scope filters by build system).
    #[arg(long, value_delimiter = ',', default_values_t = vec!["x86_64-linux".to_string(), "aarch64-linux".to_string()])]
    pub systems: Vec<String>,

    /// Local output directory; artifacts land in
    /// `<out-dir>/<hydra-eval>/<recipe-short-digest>/`.
    #[arg(long)]
    pub out_dir: PathBuf,

    /// Scratch directory (tarball download, unpack, gc-roots).
    /// Default: `<out-dir>/work`.
    #[arg(long)]
    pub work_dir: Option<PathBuf>,

    /// Upload to this S3 bucket when set (otherwise local-only).
    #[arg(long, env = "RIO_PARITY_S3_BUCKET")]
    pub s3_bucket: Option<String>,

    /// S3 key prefix.
    #[arg(long, default_value = "parity")]
    pub s3_prefix: String,

    /// Stop after the fidelity gate: no closure pass, no truth sweep, no
    /// archive, no upload.
    #[arg(long)]
    pub dry_run: bool,

    /// Record under a new recipe digest even if this recipe has already
    /// been recorded (published archives are write-once; the salted
    /// digest also bypasses the by-recipe idempotency skip).
    #[arg(long)]
    pub force: bool,

    /// Hydra instance base URL (every request to it counts against the
    /// politeness budget).
    #[arg(long, default_value = "https://hydra.nixos.org")]
    pub hydra_url: String,

    /// Binary cache swept for narinfo presence at recording time (the
    /// truth baked into the archive's expected outcomes) and recorded in
    /// the archive as the relay substituter. Must be an https:// URL.
    #[arg(long, default_value = "https://cache.nixos.org")]
    pub cache_url: String,

    /// Concurrent narinfo fetches during the truth sweep against
    /// `--cache-url`. The cache sits behind a CDN, so this is a
    /// politeness bound rather than a request budget.
    #[arg(long, default_value_t = 64)]
    pub narinfo_concurrency: usize,

    /// Contact string appended to the User-Agent (politeness).
    #[arg(long, env = "RIO_PARITY_CONTACT")]
    pub contact: Option<String>,

    /// Hard cap on hydra.nixos.org requests (auto-raised to scope size + 20
    /// for explicit job lists larger than the default).
    #[arg(long)]
    pub hydra_request_cap: Option<u32>,

    /// Override the nixpkgs tarball URL (default: derived from the
    /// jobset's git input — github.com archive URL).
    #[arg(long)]
    pub source_tarball_url: Option<String>,

    /// Override revCount (else recovered from a versionSuffix-carrying build).
    #[arg(long)]
    pub rev_count: Option<u64>,
    /// Override shortRev (else recovered alongside revCount).
    #[arg(long)]
    pub short_rev: Option<String>,
    /// Job whose nixname/releasename carries the versionSuffix, used for
    /// revCount recovery when the scoped builds don't carry one.
    #[arg(long)]
    pub version_job: Option<String>,

    /// `nix` binary used for the store-add and derivation-show
    /// subprocesses.
    #[arg(long, default_value = "nix")]
    pub nix_bin: String,
    /// `nix-eval-jobs` binary used for the scoped evaluation.
    #[arg(long, default_value = "nix-eval-jobs")]
    pub nix_eval_jobs_bin: String,
    /// Number of nix-eval-jobs evaluation worker processes.
    #[arg(long, default_value_t = 4)]
    pub eval_workers: u32,
    /// Per-worker memory threshold (MiB) passed to nix-eval-jobs as
    /// `--max-memory-size`: a worker exceeding it is restarted after
    /// finishing its current job — it is not a hard cap on evaluation
    /// memory.
    #[arg(long, default_value_t = 4096)]
    pub eval_max_memory_mb: u64,
    /// Sample size for full-scope fidelity (reserved; unused for scoped
    /// recordings, whose fidelity gate compares every job).
    #[arg(long, default_value_t = 100)]
    pub fidelity_samples: usize,
}

/// Per-path retry budget for the truth sweep. cache.nixos.org sits
/// behind a CDN, so transient 5xx/reset errors are common enough that a
/// single-shot fetch would abort large sweeps spuriously, while five
/// attempts with the sweep's exponential backoff stays well under a
/// minute per path worst-case.
const NARINFO_SWEEP_MAX_ATTEMPTS: u32 = 5;

/// Derive the per-job Hydra buildstatus map from the builds fetched
/// during scope resolution. Only finished builds carry a meaningful
/// status — an unfinished build's code describes a build that is still
/// queued or running — so entries without `finished == 1` or without a
/// status are dropped and the affected job's expected outcome falls back
/// to narinfo presence alone.
fn buildstatus_from_builds(builds: &[crate::hydra::HydraBuild]) -> BTreeMap<String, i64> {
    builds
        .iter()
        .filter(|build| build.finished == Some(1))
        .filter_map(|build| build.buildstatus.map(|status| (build.job.clone(), status)))
        .collect()
}

pub async fn run(args: EvalArgs) -> anyhow::Result<()> {
    use anyhow::Context as _;

    use crate::evalset::artifacts::{EvalSetDir, FIDELITY_FILE};
    use crate::evalset::{depclosure, evaluator, fidelity, key, outcomes, package, recipe};
    use crate::s3::{ARCHIVE_IMAGE_OBJECT, ARCHIVE_MANIFEST_OBJECT, ArchiveS3, ByRecipePointer};

    let scope = parse_scope(&args.scope)?;
    // Sorted + deduplicated once up front: the same normalized list
    // feeds the constituents system filter, the recipe key (where
    // `--systems` argument order must not fork the digest), and the
    // archive provenance.
    let systems = key::EvalSetKey::normalize_systems(args.systems.clone());
    let ua = crate::user_agent(args.contact.as_deref());
    // The cache URL is both the truth-sweep target and the archive's
    // relay substituter; the archive format only allows https:// (or
    // s3://) relays and the sweep itself fetches narinfos over HTTPS, so
    // anything else would only fail late — after the evaluation — at
    // archive staging time. Refuse it before any work is done.
    anyhow::ensure!(
        args.cache_url.starts_with("https://"),
        "--cache-url must be an https:// URL (got {:?}): it is recorded in the archive as the \
         relay substituter, which allows only https:// or s3:// URLs, and the recorder's narinfo \
         truth sweep fetches from it over HTTPS",
        args.cache_url
    );

    // ── Phase 1: Hydra structural fetches (eval, project/jobset, jobset config, scope) ──
    let scope_job_count = match &scope {
        Scope::Jobs { jobs } => jobs.len(),
        _ => 0,
    };
    // An explicit job list costs one /eval/<id>/job/<name> request per
    // job, so the default cap auto-raises to scope size + 20 structural
    // requests; --hydra-request-cap overrides either way.
    let cap = args.hydra_request_cap.unwrap_or_else(|| {
        crate::hydra::DEFAULT_HYDRA_REQUEST_CAP.max(
            u32::try_from(scope_job_count)
                .unwrap_or(u32::MAX)
                .saturating_add(20),
        )
    });
    let hydra = crate::hydra::HydraClient::new(
        &args.hydra_url,
        &ua,
        cap,
        crate::hydra::DEFAULT_HYDRA_MIN_INTERVAL,
    )?;

    let eval = hydra.get_eval(args.hydra_eval).await?;
    let nixpkgs_input = eval
        .jobsetevalinputs
        .get("nixpkgs")
        .context("eval has no `nixpkgs` input; only nixpkgs-based jobsets are supported")?;
    let revision = nixpkgs_input
        .revision
        .clone()
        .context("eval nixpkgs input has no revision")?;

    // project/jobset: --jobset wins; otherwise sample the eval's first
    // build (one extra Hydra request), since the eval JSON itself
    // carries no project/jobset keys.
    let (project, jobset_name) = match &args.jobset {
        Some(spec) => {
            let (p, j) = spec
                .split_once('/')
                .context("--jobset must be <project>/<jobset>")?;
            (p.to_string(), j.to_string())
        }
        None => {
            let first = *eval.builds.first().context("eval has no builds")?;
            let b = hydra.get_build(first).await?;
            (
                b.project.clone().context("build JSON has no project")?,
                b.jobset.clone().context("build JSON has no jobset")?,
            )
        }
    };
    let jobset = hydra.get_jobset(&project, &jobset_name).await?;
    // Snapshot of the jobset configuration the recipe was derived from,
    // recorded verbatim in the archive provenance for auditability.
    let jobset_config = serde_json::json!({
        "project": project,
        "name": jobset_name,
        "nixexprinput": jobset.nixexprinput,
        "nixexprpath": jobset.nixexprpath,
        "inputs": jobset
            .inputs
            .iter()
            .map(|(k, v)| (k.clone(), serde_json::json!({"type": v.input_type, "value": v.value})))
            .collect::<BTreeMap<_, _>>(),
    });

    // Resolve the scope into the in-scope job list, Hydra's ground
    // truth (job → drvpath) for the fidelity gate, and the builds whose
    // names can be mined for versionSuffix recovery (the same builds
    // later feed the per-job buildstatus half of the truth sweep).
    let mut ground_truth: BTreeMap<String, String> = BTreeMap::new();
    let mut sampled_builds: Vec<crate::hydra::HydraBuild> = Vec::new();
    let in_scope_jobs: Vec<String> = match &scope {
        Scope::Constituents { aggregate_job } => {
            let agg = hydra.get_eval_job(args.hydra_eval, aggregate_job).await?;
            let constituents = hydra.get_constituents(agg.id).await?;
            let mut jobs = Vec::new();
            for b in constituents {
                // Constituent lists span systems; keep only the
                // requested ones (builds without a system field are
                // kept). The aggregate itself is excluded: Hydra
                // rewrites aggregate derivations after evaluation, so
                // it could never pass the fidelity gate as a plain job.
                let sys_ok = b
                    .system
                    .as_deref()
                    .is_none_or(|s| systems.iter().any(|w| w == s));
                if b.job != *aggregate_job && sys_ok {
                    ground_truth.insert(b.job.clone(), b.drvpath.clone());
                    jobs.push(b.job.clone());
                    sampled_builds.push(b);
                }
            }
            jobs.sort();
            jobs.dedup();
            anyhow::ensure!(
                !jobs.is_empty(),
                "aggregate {aggregate_job} has no in-scope constituents"
            );
            jobs
        }
        Scope::Jobs { jobs } => {
            for job in jobs {
                let b = hydra.get_eval_job(args.hydra_eval, job).await?;
                ground_truth.insert(b.job.clone(), b.drvpath.clone());
                sampled_builds.push(b);
            }
            jobs.clone()
        }
        Scope::Full => {
            // Full-evaluation recordings need the sampling-based
            // fidelity gate and node sizing that land with the
            // campaign-runner work; refuse rather than silently record
            // an unvalidated archive. The full-scope evaluator argv
            // builder already exists in `recipe::evaluator_argv_full`.
            anyhow::bail!("--scope full is not supported yet; use constituents:<job> or jobs:…");
        }
    };
    tracing::info!(jobs = in_scope_jobs.len(), "scope resolved");

    // ── Phase 2: source prep (tarball → unpack → nix store add) ──
    let work = args
        .work_dir
        .clone()
        .unwrap_or_else(|| args.out_dir.join("work"));
    std::fs::create_dir_all(&work).with_context(|| format!("create {}", work.display()))?;
    let tarball_url = match &args.source_tarball_url {
        Some(u) => u.clone(),
        None => recipe::tarball_url(
            nixpkgs_input
                .uri
                .as_deref()
                .context("eval nixpkgs input has no uri; pass --source-tarball-url")?,
            &revision,
        )?,
    };
    let tarball_path = work.join("nixpkgs.tar.gz");
    recipe::download_tarball(&tarball_url, &ua, &tarball_path).await?;
    tracing::info!(tarball = %tarball_path.display(), "unpacking nixpkgs tarball");
    let tree = recipe::unpack_tarball(&tarball_path, &work.join("src")).await?;
    tracing::info!(
        tree = %tree.display(),
        "adding the source tree to the local nix store (hashes the whole nixpkgs tree; takes a few minutes)"
    );
    let source_store_path = recipe::store_add_source(&args.nix_bin, &tree).await?;

    // revCount/shortRev: explicit overrides → recovered from the scoped
    // builds' names → recovered from --version-job → error. During the
    // scan, candidates whose recovered shortRev is not a prefix of the
    // eval's nixpkgs revision are skipped: a `pre<digits>.<hex>` match
    // in some package's own version string must not shadow a real
    // versionSuffix carried by another in-scope build (or fail the run
    // late at the consistency check below).
    let recover_matching = |name: Option<&str>| {
        name.and_then(recipe::recover_version_suffix)
            .filter(|v| recipe::short_rev_matches(&v.short_rev, &revision))
    };
    let recovered = sampled_builds.iter().find_map(|b| {
        recover_matching(b.releasename.as_deref())
            .or_else(|| recover_matching(b.nixname.as_deref()))
    });
    let recovered = match recovered {
        Some(v) => Some(v),
        None => match &args.version_job {
            Some(job) => {
                let b = hydra.get_eval_job(args.hydra_eval, job).await?;
                b.releasename
                    .as_deref()
                    .and_then(recipe::recover_version_suffix)
                    .or_else(|| {
                        b.nixname
                            .as_deref()
                            .and_then(recipe::recover_version_suffix)
                    })
            }
            None => None,
        },
    };
    let (rev_count, short_rev) = match (args.rev_count, args.short_rev.clone(), recovered) {
        (Some(rc), Some(sr), _) => (rc, sr),
        (_, _, Some(v)) => (
            args.rev_count.unwrap_or(v.rev_count),
            args.short_rev.clone().unwrap_or(v.short_rev),
        ),
        _ => anyhow::bail!(
            "could not recover revCount/shortRev from the scoped builds; pass \
             --version-job <versionSuffix-carrying job> (e.g. nixos.channel) or \
             --rev-count/--short-rev explicitly"
        ),
    };
    anyhow::ensure!(
        recipe::short_rev_matches(&short_rev, &revision),
        "shortRev {short_rev} is not a prefix of the eval revision {revision}; \
         the recovered versionSuffix likely came from an unrelated package version"
    );

    // ── Phase 3: scoped evaluation via nix-eval-jobs ──
    let nixexprpath = jobset
        .nixexprpath
        .clone()
        .context("jobset config has no nixexprpath")?;
    let entry_point = tree.join(&nixexprpath);
    anyhow::ensure!(
        entry_point.exists(),
        "entry point {} missing from the unpacked source tree",
        entry_point.display()
    );
    let nixpkgs_arg = recipe::NixpkgsArg {
        source_store_path: source_store_path.clone(),
        rev: revision.clone(),
        short_rev: short_rev.clone(),
        rev_count,
    };
    let extra_args = recipe::jobset_extra_args(&jobset)?;
    let gcroots = work.join("gcroots");
    std::fs::create_dir_all(&gcroots).context("create gcroots dir")?;

    let selection_path = work.join("selection.nix");
    let selection =
        recipe::selection_expr(&entry_point, &nixpkgs_arg, &extra_args, &in_scope_jobs)?;
    std::fs::write(&selection_path, &selection).context("write selection.nix")?;
    let argv = recipe::evaluator_argv_scoped(
        &selection_path,
        &tree,
        &gcroots,
        args.eval_workers,
        args.eval_max_memory_mb,
    );

    // Recipe identity (names the output directory, the provenance
    // `recipe_digest`, and the by-recipe idempotency pointer): tool
    // versions plus a hash of the exact evaluator argv and selection
    // expression.
    let nix_version = recipe::tool_version(&args.nix_bin).await?;
    let nej_version = recipe::tool_version(&args.nix_eval_jobs_bin).await?;
    let args_expr_sha256 = {
        use sha2::Digest as _;
        let mut h = sha2::Sha256::new();
        // NUL cannot appear inside an argv element, so it is an
        // unambiguous joiner for hashing the argv as one byte string.
        h.update(argv.join("\u{0}").as_bytes());
        h.update(selection.as_bytes());
        hex::encode(h.finalize())
    };
    let set_key = key::EvalSetKey {
        hydra_eval_id: args.hydra_eval,
        project: project.clone(),
        jobset: jobset_name.clone(),
        systems: systems.clone(),
        scope: scope.clone(),
        engine_version: env!("CARGO_PKG_VERSION").to_string(),
        nix_version,
        nix_eval_jobs_version: nej_version,
        args_expr_sha256,
        forced_at: args.force.then(|| jiff::Timestamp::now().to_string()),
    };
    let recipe_digest = set_key.digest();
    let short_digest = set_key.short_digest();
    let dir = EvalSetDir::create(
        &args
            .out_dir
            .join(args.hydra_eval.to_string())
            .join(&short_digest),
    )?;
    tracing::info!(dir = %dir.root.display(), "recorder output directory");

    let eval_out = evaluator::run_evaluator(
        &args.nix_eval_jobs_bin,
        &argv,
        &dir.path("nix-eval-jobs.stderr.log"),
    )
    .await?;
    let mut manifest = eval_out.manifest;
    let eval_errors = eval_out.errors;
    let aggregates = eval_out.aggregates;
    tracing::info!(
        manifest = manifest.len(),
        eval_errors = eval_errors.len(),
        aggregates = aggregates.len(),
        "evaluation complete"
    );

    // ── Phase 4: fidelity gate (exhaustive — scoped recordings compare every job) ──
    let report = fidelity::compare_drv_paths(&manifest, &ground_truth, fidelity::MODE_EXHAUSTIVE);
    dir.write_json(FIDELITY_FILE, &report)?;
    if report.divergent {
        tracing::error!(
            mismatches = report.mismatches.len(),
            "fidelity gate FAILED: locally produced drvPaths diverge from Hydra's; the archive will be marked divergent"
        );
    } else {
        tracing::info!(
            checked = report.checked,
            matched = report.matched,
            "fidelity gate passed"
        );
    }

    // The provenance block carried verbatim in the archive manifest (and,
    // for --dry-run, written locally as provenance.json): the full
    // reproduction recipe and its digest, where the evaluation came from,
    // how the evaluator was invoked, the fidelity verdict, and recording
    // statistics. Counts that only exist once the closure pass and the
    // packer have run are passed in by the caller; `mkdwarfs_version` is
    // None on a dry run, where no image is packed.
    let manifest_records = manifest.len();
    let build_provenance = |closure_drvs: usize,
                            ca_outputs: usize,
                            hydra_requests_used: u32,
                            mkdwarfs_version: Option<&str>,
                            dry_run: bool|
     -> serde_json::Value {
        let mut provenance = serde_json::json!({
            "recorder": "rio-parity-eval",
            "recorder_version": env!("CARGO_PKG_VERSION"),
            "recipe_digest": &recipe_digest,
            "recipe": &set_key,
            "source": {
                "kind": "hydra",
                "hydra_eval_id": args.hydra_eval,
                "project": &project,
                "jobset": &jobset_name,
                "nixpkgs_revision": &revision,
                "rev_count": rev_count,
                "short_rev": &short_rev,
                "source_store_path": &source_store_path,
                "jobset_config": &jobset_config,
            },
            "evaluator": {
                "program": &args.nix_eval_jobs_bin,
                "argv": &argv,
            },
            "fidelity": {
                "mode": &report.mode,
                "checked": report.checked,
                "matched": report.matched,
                "mismatch_count": report.mismatches.len(),
                "divergent": report.divergent,
            },
            "stats": {
                "in_scope_jobs": in_scope_jobs.len(),
                "manifest_records": manifest_records,
                "eval_errors": eval_errors.len(),
                "aggregates_excluded": aggregates.len(),
                "closure_drvs": closure_drvs,
                "ca_outputs": ca_outputs,
                "hydra_requests_used": hydra_requests_used,
            },
            "systems": &systems,
            "scope": &scope,
            "mkdwarfs_version": mkdwarfs_version,
        });
        if dry_run {
            provenance["dry_run"] = serde_json::Value::Bool(true);
        }
        provenance
    };

    if args.dry_run {
        // Stop after the fidelity gate; the locally written fidelity
        // report plus a dry-run-marked provenance document keep the run
        // auditable without staging an archive.
        let hydra_requests_used = hydra.requests_used().await;
        let provenance = build_provenance(0, 0, hydra_requests_used, None, true);
        let provenance_path = dir.write_json("provenance.json", &provenance)?;
        tracing::info!(
            provenance = %provenance_path.display(),
            "--dry-run: stopping after the fidelity gate (no closure pass, no truth sweep, no archive, no upload)"
        );
    } else {
        // ── Phase 5: closure adjacency + drv members ──
        // One `nix derivation show -r` per workload unit; the per-target
        // adjacency views merge key-by-key into one union over the whole
        // scope (a derivation reached from several targets yields
        // identical records, so insert-overwrite is a no-op for repeats).
        let total = manifest.len();
        tracing::info!(
            targets = total,
            "extracting closure adjacency (one `nix derivation show -r` per workload unit)"
        );
        let mut adjacency = depclosure::ClosureAdjacency::default();
        for (idx, rec) in manifest.iter_mut().enumerate() {
            let show = depclosure::run_derivation_show(&args.nix_bin, &rec.drv_path).await?;
            let target = depclosure::closure_adjacency_from_show_json(&show)?;
            adjacency.records.extend(target.records);
            adjacency.impure_env.extend(target.impure_env);
            adjacency
                .required_system_features
                .extend(target.required_system_features);
            // Backfill the unit's requiredFeatures from the derivation's
            // own requiredSystemFeatures declaration (nix-eval-jobs does
            // not emit it).
            rec.required_features = adjacency
                .required_system_features
                .get(&rec.drv_path)
                .cloned();
            // Each record is one subprocess, so a large scope spends a
            // long time in this loop; log every 25 records (and at the
            // end) so an operator can see it is still moving.
            let done = idx + 1;
            if done % 25 == 0 || done == total {
                tracing::info!(done, total, job = %rec.job, "closure adjacency progress");
            }
        }
        let closure_drvs = adjacency.records.len();
        let ca_outputs: usize = adjacency
            .records
            .values()
            .map(|rec| rec.outputs.values().filter(|path| path.is_none()).count())
            .sum();
        tracing::info!(closure_drvs, ca_outputs, "closure adjacency union complete");

        // ── Phase 6: truth sweep (cache narinfo presence + Hydra buildstatus) ──
        let buildstatus = buildstatus_from_builds(&sampled_builds);
        let output_paths: Vec<String> = manifest
            .iter()
            .flat_map(|rec| rec.outputs.values().cloned())
            .collect();
        let cache_client = crate::nixcache::NixCacheClient::new(&args.cache_url, &ua)?;
        let truth_swept_at = jiff::Timestamp::now();
        tracing::info!(
            paths = output_paths.len(),
            cache = %args.cache_url,
            "sweeping upstream narinfo presence for expected outcomes"
        );
        let facts = crate::nixcache::sweep_narinfos(
            &cache_client,
            &output_paths,
            args.narinfo_concurrency,
            NARINFO_SWEEP_MAX_ATTEMPTS,
        )
        .await?;
        let outcome_records: Vec<_> = manifest
            .iter()
            .map(|rec| {
                outcomes::expected_outcome_for_unit(
                    &rec.drv_path,
                    &rec.outputs,
                    &facts,
                    buildstatus.get(&rec.job).copied(),
                )
            })
            .collect();

        // ── Phase 7: archive staging + mkdwarfs pack ──
        let mkdwarfs_version = package::mkdwarfs_version().await?;
        let hydra_requests_used = hydra.requests_used().await;
        let provenance = build_provenance(
            closure_drvs,
            ca_outputs,
            hydra_requests_used,
            Some(mkdwarfs_version.as_str()),
            false,
        );
        let staging_dir = dir.path("archive");
        let stage_inputs = package::StageInputs {
            manifest: &manifest,
            eval_errors: &eval_errors,
            aggregates: &aggregates,
            adjacency: &adjacency,
            outcomes: outcome_records,
            fidelity: &report,
            provenance,
            relay_substituters: vec![args.cache_url.clone()],
            truth_swept_at,
            drv_text_overrides: BTreeMap::new(),
        };
        let staged = package::stage_archive(&staging_dir, &stage_inputs)
            .await
            .context("stage the v1 replay archive from the evaluation outputs")?;
        tracing::info!(
            archive_id = %staged.archive_id,
            staging = %staged.dir.display(),
            "archive staged"
        );

        let image_path = dir.path(ARCHIVE_IMAGE_OBJECT);
        {
            // mkdwarfs is a long-running external process driven
            // synchronously by the packer; run it off the async executor.
            let staging = staging_dir.clone();
            let image = image_path.clone();
            tokio::task::spawn_blocking(move || {
                crate::archive::writer::pack_with_mkdwarfs(&staging, &image)
            })
            .await
            .context("join the mkdwarfs packing task")??;
        }
        // The standalone manifest copy next to the image is what the S3
        // publish reads (and what operators inspect without mounting the
        // image).
        let local_manifest = dir.path(ARCHIVE_MANIFEST_OBJECT);
        std::fs::copy(
            staging_dir.join(crate::archive::MANIFEST_MEMBER),
            &local_manifest,
        )
        .with_context(|| format!("copy the staged manifest to {}", local_manifest.display()))?;
        tracing::info!(image = %image_path.display(), "archive packed");

        // ── Phase 8: S3 upload with by-recipe idempotency (skipped when no bucket is configured) ──
        if let Some(bucket) = &args.s3_bucket {
            let client =
                rio_common::s3::default_client(rio_common::s3::DEFAULT_S3_MAX_ATTEMPTS).await;
            let layout = ArchiveS3::new(bucket, &args.s3_prefix);
            // Idempotency: a recipe that already produced a published
            // archive is not re-uploaded. --force salts the recipe key, so
            // a forced re-record looks up (and later writes) a different
            // pointer and never trips this skip. A pointer that does not
            // actually name an archive (drift-tolerated reads turn garbage
            // into empty fields) is ignored and overwritten by re-recording.
            let already_recorded = if args.force {
                false
            } else {
                match layout
                    .read_by_recipe_pointer(&client, &recipe_digest)
                    .await?
                {
                    Some(pointer) if pointer.names_archive() => {
                        let exists = layout
                            .archive_exists(&client, &pointer.archive_id_short)
                            .await?;
                        if exists {
                            tracing::info!(
                                archive_id = %pointer.archive_id,
                                archive_id_short = %pointer.archive_id_short,
                                "recipe already recorded; skipping upload"
                            );
                        }
                        exists
                    }
                    _ => false,
                }
            };
            if !already_recorded {
                let uploader = format!("rio-parity-eval/{}", env!("CARGO_PKG_VERSION"));
                let uploaded = layout
                    .upload_archive(
                        &client,
                        &dir.root,
                        &staged.archive_id,
                        &staged.archive_id_short,
                        &uploader,
                    )
                    .await?;
                tracing::info!(
                    objects = uploaded.len(),
                    bucket = %bucket,
                    archive_id_short = %staged.archive_id_short,
                    "archive uploaded"
                );
                // The pointer is written only after the publish succeeded:
                // it must never name an archive whose complete.json is not
                // in place.
                let pointer = ByRecipePointer {
                    archive_id: staged.archive_id.clone(),
                    archive_id_short: staged.archive_id_short.clone(),
                    recorded_at: jiff::Timestamp::now().to_string(),
                };
                layout
                    .write_by_recipe_pointer(&client, &recipe_digest, &pointer)
                    .await?;
            }
        } else {
            tracing::info!("no --s3-bucket configured; the archive is local-only");
        }
    }

    if report.divergent {
        anyhow::bail!(
            "evaluation DIVERGENT: {} drvPath mismatches (see {})",
            report.mismatches.len(),
            dir.path(FIDELITY_FILE).display()
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::*;

    #[test]
    fn parses_scopes() {
        assert_eq!(parse_scope("full").unwrap(), Scope::Full);
        assert_eq!(
            parse_scope("constituents:tested").unwrap(),
            Scope::Constituents {
                aggregate_job: "tested".into()
            }
        );
        assert_eq!(
            parse_scope("jobs:nixpkgs.hello.x86_64-linux, nixpkgs.jq.x86_64-linux").unwrap(),
            Scope::Jobs {
                jobs: vec![
                    "nixpkgs.hello.x86_64-linux".into(),
                    "nixpkgs.jq.x86_64-linux".into()
                ]
            }
        );
        assert!(parse_scope("constituents:").is_err());
        assert!(parse_scope("jobs:").is_err());
        assert!(parse_scope("bogus").is_err());
    }

    #[test]
    fn job_list_scopes_are_sorted_and_deduplicated() {
        // Repetition or ordering of the same job list must not fork the
        // recipe key digest, double up per-job Hydra requests, or define
        // the same selection.nix attribute twice.
        assert_eq!(
            parse_scope(
                "jobs:nixpkgs.jq.x86_64-linux,nixpkgs.hello.x86_64-linux,nixpkgs.jq.x86_64-linux"
            )
            .unwrap(),
            Scope::Jobs {
                jobs: vec![
                    "nixpkgs.hello.x86_64-linux".into(),
                    "nixpkgs.jq.x86_64-linux".into()
                ]
            }
        );
        // jobs-file goes through the same normalization.
        let tmp = tempfile::tempdir().unwrap();
        let f = tmp.path().join("jobs.txt");
        std::fs::write(
            &f,
            "nixpkgs.jq.x86_64-linux\nnixpkgs.hello.x86_64-linux\nnixpkgs.jq.x86_64-linux\n",
        )
        .unwrap();
        assert_eq!(
            parse_scope(&format!("jobs-file:{}", f.display())).unwrap(),
            Scope::Jobs {
                jobs: vec![
                    "nixpkgs.hello.x86_64-linux".into(),
                    "nixpkgs.jq.x86_64-linux".into()
                ]
            }
        );
    }

    #[test]
    fn parses_jobs_file_scope() {
        let tmp = tempfile::tempdir().unwrap();
        let f = tmp.path().join("jobs.txt");
        std::fs::write(
            &f,
            "# comment\nnixpkgs.hello.x86_64-linux\n\nnixpkgs.jq.x86_64-linux\n",
        )
        .unwrap();
        assert_eq!(
            parse_scope(&format!("jobs-file:{}", f.display())).unwrap(),
            Scope::Jobs {
                jobs: vec![
                    "nixpkgs.hello.x86_64-linux".into(),
                    "nixpkgs.jq.x86_64-linux".into()
                ]
            }
        );
        assert!(parse_scope("jobs-file:/does/not/exist").is_err());
    }

    #[test]
    fn cli_args_parse_with_defaults() {
        use clap::Parser as _;
        #[derive(clap::Parser)]
        struct T {
            #[command(flatten)]
            eval: EvalArgs,
        }
        let t = T::parse_from([
            "t",
            "--hydra-eval",
            "1824219",
            "--scope",
            "jobs:nixpkgs.hello.x86_64-linux",
            "--out-dir",
            "/tmp/out",
        ]);
        assert_eq!(t.eval.hydra_eval, 1824219);
        assert_eq!(t.eval.s3_prefix, "parity");
        assert_eq!(t.eval.eval_workers, 4);
        assert_eq!(t.eval.narinfo_concurrency, 64);
        assert_eq!(
            t.eval.systems,
            vec!["x86_64-linux".to_string(), "aarch64-linux".to_string()]
        );
        assert!(!t.eval.dry_run);
    }

    /// A Hydra build record with only the fields the buildstatus map
    /// derivation looks at varying; everything else is fixed filler.
    fn build(
        job: &str,
        buildstatus: Option<i64>,
        finished: Option<i64>,
    ) -> crate::hydra::HydraBuild {
        crate::hydra::HydraBuild {
            id: 1,
            project: None,
            jobset: None,
            job: job.to_string(),
            system: Some("x86_64-linux".to_string()),
            drvpath: format!("/nix/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-{job}.drv"),
            buildoutputs: BTreeMap::new(),
            buildstatus,
            finished,
            nixname: None,
            releasename: None,
            jobsetevals: Vec::new(),
        }
    }

    #[test]
    fn buildstatus_map_from_sampled_builds_only_uses_finished_builds() {
        let builds = vec![
            build("hello.x86_64-linux", Some(0), Some(1)),
            build("jq.x86_64-linux", Some(1), Some(1)),
            build("running.x86_64-linux", Some(0), Some(0)),
            build("queued.x86_64-linux", None, Some(1)),
            build("no-finished.x86_64-linux", Some(2), None),
        ];
        let map = buildstatus_from_builds(&builds);
        assert_eq!(map.len(), 2, "only finished builds with a status survive");
        assert_eq!(map["hello.x86_64-linux"], 0);
        assert_eq!(map["jq.x86_64-linux"], 1);
        assert!(!map.contains_key("running.x86_64-linux"));
        assert!(!map.contains_key("queued.x86_64-linux"));
        assert!(!map.contains_key("no-finished.x86_64-linux"));
    }
}
