//! `rio-parity eval` — build an eval set from a Hydra evaluation: the
//! job manifest, the drvPath fidelity report, the dependency closure,
//! and the packed derivation archive.
//!
//! Phases: Hydra structural fetches → source prep (tarball download,
//! unpack, `nix store add-path`) → scoped nix-eval-jobs run → drvPath
//! fidelity gate → dep-closure enumeration → drv archive →
//! evalset.json → optional S3 upload. `--dry-run` stops after the
//! fidelity gate: no dep-closure, no archive, no upload (evalset.json
//! is still written locally, marked `dry_run`, so the run is
//! auditable).

use std::path::PathBuf;

use clap::Args;

use crate::evalset::Scope;

/// Parse `--scope`:
///   `full`
///   `constituents:<aggregate-job>`   (e.g. constituents:tested)
///   `jobs:<job1,job2,…>`
///   `jobs-file:<path>`               (one job name per line, `#` comments)
///
/// Aggregate jobs (e.g. `tested`) must be requested via
/// `constituents:<job>`, never listed under `jobs:`/`jobs-file:`:
/// scoped evaluations run nix-eval-jobs without `--constituents`, so a
/// hand-fed aggregate evaluates as a plain job whose drvPath cannot
/// match the constituent-rewritten derivation Hydra stores, and the
/// eval set would always come out divergent.
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
        let jobs: Vec<String> = list
            .split(',')
            .map(str::trim)
            .filter(|j| !j.is_empty())
            .map(String::from)
            .collect();
        anyhow::ensure!(!jobs.is_empty(), "jobs: needs at least one job name");
        return Ok(Scope::Jobs { jobs });
    }
    if let Some(path) = s.strip_prefix("jobs-file:") {
        let text = std::fs::read_to_string(path)
            .map_err(|e| anyhow::anyhow!("read jobs file {path}: {e}"))?;
        let jobs: Vec<String> = text
            .lines()
            .map(str::trim)
            .filter(|l| !l.is_empty() && !l.starts_with('#'))
            .map(String::from)
            .collect();
        anyhow::ensure!(!jobs.is_empty(), "jobs file {path} contains no job names");
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
    /// Hydra's rewritten aggregate derivation and the eval set would
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
    /// `<out-dir>/<hydra-eval>/<key-digest>/`.
    #[arg(long)]
    pub out_dir: PathBuf,

    /// Scratch directory (tarball download, unpack, gc-roots, drv layout).
    /// Default: `<out-dir>/work`.
    #[arg(long)]
    pub work_dir: Option<PathBuf>,

    /// Upload to this S3 bucket when set (otherwise local-only).
    #[arg(long, env = "RIO_PARITY_S3_BUCKET")]
    pub s3_bucket: Option<String>,

    /// S3 key prefix.
    #[arg(long, default_value = "parity")]
    pub s3_prefix: String,

    /// Stop after the fidelity gate: no dep-closure, no drv archive, no upload.
    #[arg(long)]
    pub dry_run: bool,

    /// Build under a new key digest even if an eval set for the same
    /// key already exists (eval sets are write-once).
    #[arg(long)]
    pub force: bool,

    #[arg(long, default_value = "https://hydra.nixos.org")]
    pub hydra_url: String,

    #[arg(long, default_value = "https://cache.nixos.org")]
    pub cache_url: String,

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

    #[arg(long, default_value = "nix")]
    pub nix_bin: String,
    #[arg(long, default_value = "nix-eval-jobs")]
    pub nix_eval_jobs_bin: String,
    #[arg(long, default_value_t = 4)]
    pub eval_workers: u32,
    #[arg(long, default_value_t = 4096)]
    pub eval_max_memory_mb: u64,
    /// Sample size for full-scope fidelity (scoped sets are exhaustive).
    #[arg(long, default_value_t = 100)]
    pub fidelity_samples: usize,
}

pub async fn run(args: EvalArgs) -> anyhow::Result<()> {
    use std::collections::BTreeMap;

    use anyhow::Context as _;

    use crate::evalset::artifacts::{
        DEP_CLOSURE_FILE, DRVS_ARCHIVE_FILE, EVAL_ERRORS_FILE, EVALSET_FILE, EvalSetDir,
        FIDELITY_FILE, MANIFEST_FILE,
    };
    use crate::evalset::{archive, depclosure, evaluator, fidelity, key, recipe};

    let scope = parse_scope(&args.scope)?;
    // Sorted + deduplicated once up front: the same normalized list
    // feeds the constituents system filter, the eval-set key (where
    // `--systems` argument order must not fork the digest), and
    // evalset.json.
    let systems = key::EvalSetKey::normalize_systems(args.systems.clone());
    let ua = crate::user_agent(args.contact.as_deref());

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
    // recorded verbatim in evalset.json for auditability.
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
    // names can be mined for versionSuffix recovery.
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
            // Full-evaluation sets need the sampling-based fidelity
            // gate and node sizing that land with the campaign-runner
            // work; refuse rather than silently build an unvalidated
            // set. The full-scope evaluator argv builder already exists
            // in `recipe::evaluator_argv_full`.
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
    let tree = recipe::unpack_tarball(&tarball_path, &work.join("src")).await?;
    let source_store_path = recipe::store_add_source(&args.nix_bin, &tree).await?;

    // revCount/shortRev: explicit overrides → recovered from the scoped
    // builds' names → recovered from --version-job → error.
    let recovered = sampled_builds.iter().find_map(|b| {
        b.releasename
            .as_deref()
            .and_then(recipe::recover_version_suffix)
            .or_else(|| {
                b.nixname
                    .as_deref()
                    .and_then(recipe::recover_version_suffix)
            })
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

    // Eval-set identity (names the output directory): tool versions
    // plus a hash of the exact evaluator argv and selection expression.
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
    let digest = set_key.short_digest();
    let dir = EvalSetDir::create(&args.out_dir.join(args.hydra_eval.to_string()).join(&digest))?;
    tracing::info!(dir = %dir.root.display(), "eval-set output directory");

    let eval_out = evaluator::run_evaluator(
        &args.nix_eval_jobs_bin,
        &argv,
        &dir.path("nix-eval-jobs.stderr.log"),
    )
    .await?;
    let mut manifest = eval_out.manifest;
    dir.write_jsonl(MANIFEST_FILE, &manifest)?;
    dir.write_jsonl(EVAL_ERRORS_FILE, &eval_out.errors)?;
    tracing::info!(
        manifest = manifest.len(),
        eval_errors = eval_out.errors.len(),
        aggregates = eval_out.aggregates.len(),
        "evaluation complete"
    );

    // ── Phase 4: fidelity gate (exhaustive — scoped sets compare every job) ──
    let report = fidelity::compare_drv_paths(&manifest, &ground_truth, fidelity::MODE_EXHAUSTIVE);
    dir.write_json(FIDELITY_FILE, &report)?;
    if report.divergent {
        tracing::error!(
            mismatches = report.mismatches.len(),
            "fidelity gate FAILED: locally produced drvPaths diverge from Hydra's; the eval set will be marked divergent"
        );
    } else {
        tracing::info!(
            checked = report.checked,
            matched = report.matched,
            "fidelity gate passed"
        );
    }

    // ── Phase 5/6: dep-closure + drv archive (skipped on --dry-run) ──
    let mut stats = key::EvalSetStats {
        in_scope_jobs: in_scope_jobs.len(),
        manifest_records: manifest.len(),
        eval_errors: eval_out.errors.len(),
        aggregates_excluded: eval_out.aggregates.len(),
        dep_closure_records: 0,
        ca_outputs: 0,
        hydra_requests_used: hydra.requests_used().await,
        archive_bytes: None,
    };
    if !args.dry_run {
        // One `nix derivation show -r` per manifest target: emits the
        // dep-closure record and backfills the manifest record's
        // requiredFeatures from the derivation's requiredSystemFeatures.
        let mut dep_records = Vec::with_capacity(manifest.len());
        for rec in &mut manifest {
            let show = depclosure::run_derivation_show(&args.nix_bin, &rec.drv_path).await?;
            let (dep, features) =
                depclosure::dep_closure_from_show_json(&show, &rec.drv_path, &rec.job)?;
            rec.required_features = features;
            stats.ca_outputs += dep.ca_outputs.len();
            dep_records.push(dep);
        }
        dir.write_jsonl(DEP_CLOSURE_FILE, &dep_records)?;
        // Re-write the manifest now that requiredFeatures is backfilled.
        dir.write_jsonl(MANIFEST_FILE, &manifest)?;
        stats.dep_closure_records = dep_records.len();

        let layout_dir = work.join("drv-layout");
        let drvs: Vec<String> = manifest.iter().map(|r| r.drv_path.clone()).collect();
        archive::export_drv_closure(&args.nix_bin, &drvs, &layout_dir).await?;
        let archive_path = dir.path(DRVS_ARCHIVE_FILE);
        // pack_layout_to_tar_zst is blocking (tar subprocess + zstd
        // encode); run it off the async executor.
        let bytes = tokio::task::spawn_blocking(move || {
            archive::pack_layout_to_tar_zst(&layout_dir, &archive_path)
        })
        .await
        .context("join archive-pack task")??;
        stats.archive_bytes = Some(bytes);
    }

    // ── Phase 7: evalset.json (always written; records dry_run + the verdict) ──
    let key_digest = set_key.digest();
    let meta = key::EvalSetMeta {
        key_digest,
        key_short_digest: digest.clone(),
        key: set_key,
        hydra_eval_id: args.hydra_eval,
        nixpkgs_revision: revision,
        project,
        jobset: jobset_name,
        jobset_config,
        source_store_path,
        rev_count,
        short_rev,
        evaluator_program: args.nix_eval_jobs_bin.clone(),
        evaluator_argv: argv,
        systems,
        scope,
        dry_run: args.dry_run,
        fidelity_divergent: report.divergent,
        stats,
        created_at: jiff::Timestamp::now().to_string(),
    };
    dir.write_json(EVALSET_FILE, &meta)?;
    tracing::info!(evalset = %dir.path(EVALSET_FILE).display(), "evalset.json written");

    // ── Phase 8: S3 upload (skipped on --dry-run or when no bucket is configured) ──
    if args.dry_run {
        tracing::info!(
            "--dry-run: stopping after the fidelity gate (no dep-closure, no archive, no upload)"
        );
    } else if let Some(bucket) = &args.s3_bucket {
        let client = rio_common::s3::default_client(rio_common::s3::DEFAULT_S3_MAX_ATTEMPTS).await;
        let layout = crate::s3::EvalSetS3::new(bucket, &args.s3_prefix);
        let uploaded = layout
            .upload_eval_set(&client, &dir, args.hydra_eval, &digest)
            .await?;
        tracing::info!(objects = uploaded.len(), bucket = %bucket, "eval set uploaded");
    } else {
        tracing::info!("no --s3-bucket configured; eval set is local-only");
    }

    if report.divergent {
        anyhow::bail!(
            "eval set built but DIVERGENT: {} drvPath mismatches (see {})",
            report.mismatches.len(),
            dir.path(FIDELITY_FILE).display()
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
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
        assert_eq!(
            t.eval.systems,
            vec!["x86_64-linux".to_string(), "aarch64-linux".to_string()]
        );
        assert!(!t.eval.dry_run);
    }
}
