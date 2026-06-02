//! `rio-replay eval` — record a v1 replay archive from a Hydra
//! evaluation: reproduce the evaluation locally, gate it on drvPath
//! fidelity, sweep upstream truth, and publish the packed archive.
//!
//! Phases: Hydra structural fetches → source prep (tarball download,
//! unpack, `nix store add-path`) → scoped nix-eval-jobs run → drvPath
//! fidelity gate → closure adjacency + drv members → truth sweep
//! (cache.nixos.org narinfo presence + Hydra buildstatus) → archive
//! staging + mkdwarfs pack → S3 upload. The by-recipe idempotency gate
//! runs the moment the recipe digest is computed — necessarily after
//! the Hydra fetches and source prep, whose outputs the digest hashes.
//! A re-run of an already-recorded recipe therefore still spends those
//! two phases (the politeness-budgeted Hydra requests and minutes of
//! source prep); what the gate skips is the hours-long remainder:
//! evaluation, closure pass, truth sweep, pack, and upload.
//! `--dry-run` stops after the fidelity gate: no closure pass, no truth
//! sweep, no archive, no upload (fidelity.json and a dry-run-marked
//! provenance.json are still written locally, so the run is auditable).
//!
//! The fidelity gate fails closed on zero coverage: when jobs are in
//! scope but the gate compared none of them, nothing was verified, so
//! the run aborts right after writing fidelity.json — no truth sweep,
//! no archive, no upload. (A divergent recording, by contrast, is real
//! information: it is still packed and published with its mismatched
//! units flagged `identity_divergent`, and the CLI exits non-zero at
//! the end.)

use std::collections::BTreeMap;
use std::path::PathBuf;

use clap::Args;

use crate::evalset::{Scope, recipe};

/// Parse `--scope`:
///   `full`
///   `constituents:<aggregate-job>`   (e.g. constituents:tested)
///   `jobs:<job1,job2,…>`
///   `jobs-file:<path>`               (one job name per line; `#` starts
///                                     a whole-line or trailing comment)
///
/// Explicit job lists (`jobs:`/`jobs-file:`) are sorted and
/// deduplicated here, so listing the same jobs in a different order or
/// with repetitions cannot fork the recipe key digest, issue duplicate
/// per-job Hydra requests, or define the same selection.nix attribute
/// twice (a Nix "attribute already defined" eval failure).
///
/// Every job name accepted here — list members and the `constituents:`
/// aggregate alike — is charset-validated at this input boundary
/// ([`recipe::validate_job_name`]), so a stray character fails in
/// milliseconds with the offending component named, instead of mangling
/// a politeness-budgeted `eval/<id>/job/<name>` request in Phase 1 (a
/// `#` truncates the URL at the fragment, silently querying a different
/// job) and only being rejected by the Phase-3 selection-expression
/// gate after the multi-minute source prep.
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
        recipe::validate_job_name(job)?;
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
        for job in &jobs {
            recipe::validate_job_name(job)?;
        }
        return Ok(Scope::Jobs { jobs });
    }
    if let Some(path) = s.strip_prefix("jobs-file:") {
        let text = std::fs::read_to_string(path)
            .map_err(|e| anyhow::anyhow!("read jobs file {path}: {e}"))?;
        // The shared jobs-file grammar ([`crate::jobsfile`]) — the same
        // parse the campaign engine's `filters.jobs_file` allowlist
        // uses, so a file this recorder accepts can never be refused at
        // the plan stage over comment handling.
        let mut jobs = crate::jobsfile::parse_jobs_file_lines(&text);
        anyhow::ensure!(!jobs.is_empty(), "jobs file {path} contains no job names");
        jobs.sort();
        jobs.dedup();
        for job in &jobs {
            recipe::validate_job_name(job)?;
        }
        return Ok(Scope::Jobs { jobs });
    }
    anyhow::bail!(
        "unrecognized --scope {s:?}; expected full, constituents:<job>, jobs:<j1,j2,…>, or jobs-file:<path>"
    )
}

/// Validate `--version-job` at the argument boundary. The job name is
/// embedded verbatim in the `eval/<id>/job/<name>` Hydra request issued
/// by the revCount-recovery fallback, so it gets the same charset rule
/// as every other operator-supplied job name
/// ([`recipe::validate_job_name`]): a stray character fails in
/// milliseconds with the flag named, instead of mangling a
/// politeness-budgeted recovery request issued only after the
/// multi-minute source prep (a `#` truncates the URL at the fragment,
/// silently querying a different job).
fn validate_version_job(version_job: Option<&str>) -> anyhow::Result<()> {
    match version_job {
        Some(job) => recipe::validate_job_name(job)
            .map_err(|e| anyhow::anyhow!("invalid --version-job: {e:#}")),
        None => Ok(()),
    }
}

/// Parse and validate `--jobset <project>/<jobset>` at the argument
/// boundary. Both components are interpolated verbatim into the
/// `jobset/<project>/<jobset>` Hydra request path, so each must carry
/// the same charset proof as every other Hydra-bound name
/// ([`recipe::HydraName`]; dots stay legal — real jobset names like
/// `release-25.05` contain them). Without the gate, a `#` or `?` (an
/// easy slip given flake-style `attr#output` syntax) would not fail
/// the request: `Url::join` parses the rest as a fragment/query that
/// is never transmitted, so Hydra silently serves a DIFFERENT jobset
/// whose configuration then seeds the recipe and is recorded in
/// provenance under the operator-typed name.
fn parse_jobset_spec(spec: &str) -> anyhow::Result<(recipe::HydraName, recipe::HydraName)> {
    use anyhow::Context as _;
    let (project, jobset) = spec
        .split_once('/')
        .context("--jobset must be <project>/<jobset>")?;
    let parse = |what: &str, component: &str| {
        recipe::HydraName::parse(component)
            .with_context(|| format!("--jobset {what} component {component:?}"))
    };
    Ok((parse("project", project)?, parse("jobset", jobset)?))
}

/// The constituents arm's receipt-time split of Hydra's response:
/// the in-scope constituent builds plus the constituents the recorder
/// must exclude because their names cannot be embedded.
struct ResolvedConstituents {
    /// Constituent builds kept in scope, in response order.
    in_scope: Vec<crate::hydra::HydraBuild>,
    /// `(job, why)` for in-system constituents whose names fail the
    /// Hydra-name charset rule; recorded as `unsupported` exclusions.
    unsupported: Vec<(String, String)>,
}

/// Filter the Hydra-returned constituent list of `aggregate_job` down
/// to the in-scope builds, validating every constituent name AT
/// RECEIPT: Hydra job names can legitimately contain quoted attribute
/// components (`nodePackages."@angular/cli".x86_64-linux`) that the
/// recorder can embed in neither a request path nor the generated
/// selection expression. Validating here — before any name reaches
/// `ground_truth` or the per-job request loop — turns what was a
/// deterministic Phase-3 selection-expression failure (after the
/// politeness budget and the multi-minute source prep were spent, on
/// every retry) into an exclusion record: an operator cannot edit an
/// aggregate's constituent list, so bailing would make the aggregate
/// permanently unrecordable with no workaround, while excluding keeps
/// it recordable with the gap accounted for in `exclusions.jsonl`.
///
/// The system filter runs first (builds without a system field are
/// kept) and the aggregate itself is excluded: Hydra rewrites
/// aggregate derivations after evaluation, so it could never pass the
/// fidelity gate as a plain job. Off-system constituents are skipped
/// without name validation — they are out of scope regardless.
fn resolve_constituents(
    aggregate_job: &str,
    constituents: Vec<crate::hydra::HydraBuild>,
    systems: &[String],
) -> ResolvedConstituents {
    let mut resolved = ResolvedConstituents {
        in_scope: Vec::new(),
        unsupported: Vec::new(),
    };
    for b in constituents {
        let sys_ok = b
            .system
            .as_deref()
            .is_none_or(|s| systems.iter().any(|w| w == s));
        if b.job == *aggregate_job || !sys_ok {
            continue;
        }
        match recipe::validate_job_name(&b.job) {
            Ok(()) => resolved.in_scope.push(b),
            Err(e) => resolved.unsupported.push((b.job, format!("{e:#}"))),
        }
    }
    resolved
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
    /// build of the eval (one extra Hydra request). Both components are
    /// charset-validated at the parse boundary (`parse_jobset_spec`)
    /// before they can reach a Hydra request path.
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
    #[arg(long, env = "RIO_REPLAY_S3_BUCKET")]
    pub s3_bucket: Option<String>,

    /// S3 key prefix.
    #[arg(long, default_value = "replay")]
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
    #[arg(long, env = "RIO_REPLAY_CONTACT")]
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

/// Error for a vacuous fidelity gate: jobs were in scope but the
/// comparison joined none of them, so the recording verified nothing
/// and must not proceed. The message names both sides of the empty join
/// — counts plus one example name each — because the classic cause is a
/// job-name format skew (one side's names carrying decoration the other
/// side's lack), which is immediately visible from a sample pair.
fn vacuous_gate_error(
    report: &crate::evalset::fidelity::FidelityReport,
    fidelity_path: &std::path::Path,
) -> anyhow::Error {
    use std::fmt::Write as _;

    // With zero comparisons, every job on either side sits in a
    // coverage-gap list, so the gap lists are the per-side universes.
    let mut msg = String::from("fidelity gate is vacuous: 0 in-scope jobs were compared. ");
    let _ = write!(
        msg,
        "Hydra ground truth carries {} job name(s)",
        report.missing_locally.len()
    );
    if let Some(example) = report.missing_locally.first() {
        let _ = write!(msg, " (e.g. {example:?})");
    }
    let _ = write!(
        msg,
        " and the local evaluation produced {} manifest record(s)",
        report.missing_on_hydra.len()
    );
    if let Some(example) = report.missing_on_hydra.first() {
        let _ = write!(msg, " (e.g. {example:?})");
    }
    let _ = write!(
        msg,
        ", but the job-name join matched none of them, so nothing was verified. \
         Refusing to record unverified truth; compare the two name lists in {}",
        fidelity_path.display()
    );
    anyhow::anyhow!(msg)
}

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

/// Name prefix of the per-attempt archive staging scratch dirs created in
/// the recording's output directory. Anything matching it is a dead
/// previous attempt: live scratch is only ever owned by the running
/// process (a `TempDir` guard), and the publish step renames it away.
const STAGING_SCRATCH_PREFIX: &str = ".archive-staging-";

/// Remove dead staging scratch dirs from a recording's output directory —
/// attempts killed before their `TempDir` cleanup ran (SIGKILL, node
/// loss). Without the sweep, repeated failed attempts on the surviving
/// /work volume would accumulate one multi-GB staged tree each.
fn sweep_staging_scratch(root: &std::path::Path) -> anyhow::Result<()> {
    use anyhow::Context as _;
    let entries = match std::fs::read_dir(root) {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e).with_context(|| format!("read {}", root.display())),
    };
    for entry in entries {
        let entry = entry.with_context(|| format!("read an entry of {}", root.display()))?;
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        if name.starts_with(STAGING_SCRATCH_PREFIX) && entry.path().is_dir() {
            tracing::info!(
                scratch = %entry.path().display(),
                "removing staging scratch left by a previous attempt"
            );
            std::fs::remove_dir_all(entry.path())
                .with_context(|| format!("remove stale scratch {}", entry.path().display()))?;
        }
    }
    Ok(())
}

/// Publish a fully staged archive scratch dir at its deterministic final
/// path with an atomic rename, discarding whatever a previous attempt
/// left there first (every attempt regenerates the full staging contents
/// from in-memory state, so a leftover — half-written or finalized — is
/// never worth keeping). The `TempDir` guard is disarmed only right
/// before the rename: any earlier failure cleans the scratch up.
fn publish_staged_dir(
    scratch: tempfile::TempDir,
    final_dir: &std::path::Path,
) -> anyhow::Result<()> {
    use anyhow::Context as _;
    if final_dir.exists() {
        std::fs::remove_dir_all(final_dir).with_context(|| {
            format!(
                "remove the previous attempt's staging dir {}",
                final_dir.display()
            )
        })?;
    }
    let scratch = scratch.keep();
    std::fs::rename(&scratch, final_dir).with_context(|| {
        format!(
            "rename staged archive {} to {}",
            scratch.display(),
            final_dir.display()
        )
    })
}

pub async fn run(args: EvalArgs) -> anyhow::Result<()> {
    use anyhow::Context as _;

    use crate::evalset::artifacts::{EvalSetDir, FIDELITY_FILE};
    use crate::evalset::{depclosure, evaluator, fidelity, key, outcomes, package};
    use crate::s3::{ARCHIVE_IMAGE_OBJECT, ARCHIVE_MANIFEST_OBJECT, ArchiveS3, ByRecipePointer};

    let scope = parse_scope(&args.scope)?;
    validate_version_job(args.version_job.as_deref())?;
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
    // carries no project/jobset keys. Both sources produce validated
    // [`recipe::HydraName`]s — the sampled build's values are an API
    // response, and the URL chokepoint demands the same proof of them
    // as of operator input.
    let (project, jobset_name) = match &args.jobset {
        Some(spec) => parse_jobset_spec(spec)?,
        None => {
            let first = *eval.builds.first().context("eval has no builds")?;
            let b = hydra.get_build(first).await?;
            let project = b.project.clone().context("build JSON has no project")?;
            let jobset = b.jobset.clone().context("build JSON has no jobset")?;
            (
                recipe::HydraName::parse(&project)
                    .with_context(|| format!("project name reported by sampled build {first}"))?,
                recipe::HydraName::parse(&jobset)
                    .with_context(|| format!("jobset name reported by sampled build {first}"))?,
            )
        }
    };
    let jobset = hydra.get_jobset(&project, &jobset_name).await?;
    // Snapshot of the jobset configuration the recipe was derived from,
    // recorded verbatim in the archive provenance for auditability.
    let jobset_config = serde_json::json!({
        "project": project.as_str(),
        "name": jobset_name.as_str(),
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
    // `unsupported_constituents` collects Hydra-returned constituent
    // names the recorder cannot embed; they are staged as `unsupported`
    // exclusions so the aggregate stays recordable with the gap
    // accounted for.
    let mut ground_truth: BTreeMap<String, String> = BTreeMap::new();
    let mut sampled_builds: Vec<crate::hydra::HydraBuild> = Vec::new();
    let mut unsupported_constituents: Vec<(String, String)> = Vec::new();
    let in_scope_jobs: Vec<String> = match &scope {
        Scope::Constituents { aggregate_job } => {
            // parse_scope validated the aggregate name; the chokepoint
            // type re-proves it where the URL is built.
            let aggregate = recipe::HydraName::parse(aggregate_job)?;
            let agg = hydra.get_eval_job(args.hydra_eval, &aggregate).await?;
            let constituents = hydra.get_constituents(agg.id).await?;
            let resolved = resolve_constituents(aggregate_job, constituents, &systems);
            for (job, why) in &resolved.unsupported {
                tracing::warn!(
                    job = %job,
                    why = %why,
                    "constituent name cannot be embedded; recording it as an unsupported exclusion"
                );
            }
            unsupported_constituents = resolved.unsupported;
            let mut jobs = Vec::new();
            for b in resolved.in_scope {
                ground_truth.insert(b.job.clone(), b.drvpath.clone());
                jobs.push(b.job.clone());
                sampled_builds.push(b);
            }
            jobs.sort();
            jobs.dedup();
            anyhow::ensure!(
                !jobs.is_empty(),
                "aggregate {aggregate_job} has no in-scope constituents{}",
                if unsupported_constituents.is_empty() {
                    ""
                } else {
                    " (every in-system constituent has an unsupported name; see the warnings above)"
                }
            );
            jobs
        }
        Scope::Jobs { jobs } => {
            for job in jobs {
                // Validated at parse_scope; re-proved at the chokepoint.
                let name = recipe::HydraName::parse(job)?;
                let b = hydra.get_eval_job(args.hydra_eval, &name).await?;
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
                // Validated by validate_version_job at the argument
                // boundary; re-proved at the chokepoint.
                let name = recipe::HydraName::parse(job)?;
                let b = hydra.get_eval_job(args.hydra_eval, &name).await?;
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
        project: project.to_string(),
        jobset: jobset_name.to_string(),
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

    // By-recipe idempotency, evaluated the moment the recipe digest exists.
    // The digest hashes Phase-1/2 products (project/jobset, an evaluator
    // argv and selection that embed source_store_path), so the gate
    // structurally cannot run earlier than this: a re-run of an
    // already-recorded recipe has already re-spent Phase 1's
    // politeness-budgeted Hydra requests and Phase 2's multi-minute
    // source prep by the time it exits here. What the skip saves is
    // everything downstream — the hours the evaluator, the closure pass,
    // the truth sweep, and the pack/upload would spend. It saves no Hydra
    // budget: no request is issued after this point (the truth sweep's
    // buildstatus half reuses the builds Phase 1 already fetched).
    // --force salts the recipe key, so a forced re-record reads a
    // different pointer and never trips this skip; --dry-run never
    // uploads, so the gate does not apply (a dry run of an
    // already-recorded recipe is still a useful fidelity check).
    if !args.dry_run
        && !args.force
        && let Some(bucket) = &args.s3_bucket
    {
        let client = rio_common::s3::default_client(rio_common::s3::DEFAULT_S3_MAX_ATTEMPTS).await;
        let layout = ArchiveS3::new(bucket, &args.s3_prefix);
        if let Some(pointer) = layout
            .read_by_recipe_pointer(&client, &recipe_digest)
            .await?
            && pointer.names_archive()
            && layout
                .archive_exists(&client, &pointer.archive_id_short)
                .await?
        {
            tracing::info!(
                archive_id = %pointer.archive_id,
                archive_id_short = %pointer.archive_id_short,
                "recipe already recorded; nothing to do (re-record under a fresh archive id with --force)"
            );
            return Ok(());
        }
    }

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
    // The verdict, not the divergent flag, decides the gate: "passed"
    // structurally requires a nonzero coverage witness, so a comparison
    // that joined zero jobs cannot slip through as success.
    let verdict = report.verdict();
    match verdict {
        fidelity::FidelityVerdict::Divergent { mismatches } => {
            // Real, verified counter-evidence: keep recording so the
            // divergence is published flagged (units carry
            // identity_divergent), and exit non-zero at the end.
            tracing::error!(
                mismatches = mismatches.get(),
                "fidelity gate FAILED: locally produced drvPaths diverge from Hydra's; the archive will be marked divergent"
            );
        }
        fidelity::FidelityVerdict::Passed { checked } => {
            tracing::info!(
                checked = checked.get(),
                matched = report.matched,
                "fidelity gate passed"
            );
        }
        fidelity::FidelityVerdict::Vacuous { .. } => {
            // Zero coverage with jobs in scope: nothing was verified, so
            // nothing may be recorded. Abort before the truth sweep and
            // the archive staging; fidelity.json (already written) holds
            // both job-name lists for the post-mortem.
            return Err(vacuous_gate_error(&report, &dir.path(FIDELITY_FILE)));
        }
        fidelity::FidelityVerdict::NothingInScope => {
            // Unreachable here: scope resolution guarantees at least one
            // in-scope job and each contributes ground truth. Refuse
            // rather than stage an empty archive if a future change
            // breaks that guarantee.
            anyhow::bail!(
                "fidelity gate compared an empty universe (no Hydra ground truth and no \
                 manifest records) — the recorder requires a non-empty scope"
            );
        }
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
            "recorder": "rio-replay-eval",
            "recorder_version": env!("CARGO_PKG_VERSION"),
            "recipe_digest": &recipe_digest,
            "recipe": &set_key,
            "source": {
                "kind": "hydra",
                "hydra_eval_id": args.hydra_eval,
                "project": project.as_str(),
                "jobset": jobset_name.as_str(),
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
                "constituents_unsupported": unsupported_constituents.len(),
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
        let cache_client = crate::nixcache::NixCacheClient::new(
            &crate::nixcache::CacheUrl::parse(&args.cache_url)
                .with_context(|| format!("--cache-url {:?}", args.cache_url))?,
            &ua,
        )?;
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
        let stage_inputs = package::StageInputs {
            manifest: &manifest,
            eval_errors: &eval_errors,
            aggregates: &aggregates,
            unsupported: &unsupported_constituents,
            adjacency: &adjacency,
            outcomes: outcome_records,
            fidelity: &report,
            provenance,
            relay_substituters: vec![args.cache_url.clone()],
            truth_swept_at,
            drv_text_overrides: BTreeMap::new(),
        };
        // Every attempt stages into a FRESH scratch dir and only an atomic
        // rename publishes it at the deterministic path: the eval Job's
        // retry (container restart on the surviving /work volume) re-runs
        // this phase against whatever a failed attempt left behind, and
        // staging must never die on a half-written or already-finalized
        // leftover after hours of re-evaluation. Scratch from attempts that
        // were killed before their cleanup ran is swept first so failed
        // attempts cannot accumulate multi-GB staging trees.
        sweep_staging_scratch(&dir.root)?;
        let staging_dir = dir.path("archive");
        let scratch = tempfile::Builder::new()
            .prefix(STAGING_SCRATCH_PREFIX)
            .tempdir_in(&dir.root)
            .with_context(|| format!("create a staging scratch dir in {}", dir.root.display()))?;
        let staged = package::stage_archive(scratch.path(), &stage_inputs)
            .await
            .context("stage the v1 replay archive from the evaluation outputs")?;
        publish_staged_dir(scratch, &staging_dir)?;
        let staged = package::StagedArchive {
            dir: staging_dir.clone(),
            ..staged
        };
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

        // ── Phase 8: S3 upload (skipped when no bucket is configured) ──
        // The by-recipe idempotency gate ran up front, right after the
        // recipe digest was computed — by here the recipe was not recorded
        // when this run started, so upload unconditionally. (A recorder
        // racing this one to the same recipe publishes under a different
        // archive id — the manifest's created_at differs — and the pointer
        // is last-writer-wins, so the worst case is a duplicate archive,
        // never a corrupted one.)
        if let Some(bucket) = &args.s3_bucket {
            let client =
                rio_common::s3::default_client(rio_common::s3::DEFAULT_S3_MAX_ATTEMPTS).await;
            let layout = ArchiveS3::new(bucket, &args.s3_prefix);
            let uploader = format!("rio-replay-eval/{}", env!("CARGO_PKG_VERSION"));
            // Name the destination BEFORE any byte moves: archive ids hash
            // the manifest's created_at, so a crashed attempt is never
            // retried under the same id and this log line is the only place
            // its prefix survives — `cargo xtask replay delete <short id>`
            // removes the marker-less leftovers it names.
            tracing::info!(
                destination = %format!(
                    "s3://{bucket}/{}",
                    layout.archive_prefix(&staged.archive_id_short)
                ),
                archive_id = %staged.archive_id,
                "uploading archive"
            );
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
        } else {
            tracing::info!("no --s3-bucket configured; the archive is local-only");
        }
    }

    if let fidelity::FidelityVerdict::Divergent { mismatches } = verdict {
        anyhow::bail!(
            "evaluation DIVERGENT: {} drvPath mismatches (see {})",
            mismatches,
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
    fn staging_publish_discards_leftovers_and_renames_atomically() {
        let root = tempfile::tempdir().unwrap();
        let final_dir = root.path().join("archive");

        // A previous attempt's leftover at the final path — even a
        // FINALIZED one (manifest.json present, the exact state
        // ArchiveWriter::create refuses to restage into) — is discarded by
        // the publish step, so a retry can never die on "refusing to
        // stage" after hours of re-evaluation.
        std::fs::create_dir_all(&final_dir).unwrap();
        std::fs::write(final_dir.join("manifest.json"), b"{\"old\":true}").unwrap();
        std::fs::write(final_dir.join("stale-member.jsonl"), b"{}\n").unwrap();

        let scratch = tempfile::Builder::new()
            .prefix(STAGING_SCRATCH_PREFIX)
            .tempdir_in(root.path())
            .unwrap();
        std::fs::write(scratch.path().join("manifest.json"), b"{\"new\":true}").unwrap();
        let scratch_path = scratch.path().to_path_buf();
        publish_staged_dir(scratch, &final_dir).unwrap();

        // The final path holds exactly the fresh attempt's contents and
        // the scratch is gone (renamed, not copied).
        assert_eq!(
            std::fs::read(final_dir.join("manifest.json")).unwrap(),
            b"{\"new\":true}"
        );
        assert!(
            !final_dir.join("stale-member.jsonl").exists(),
            "the previous attempt's contents must not bleed into the published staging dir"
        );
        assert!(!scratch_path.exists());
    }

    #[test]
    fn dead_staging_scratch_is_swept() {
        let root = tempfile::tempdir().unwrap();
        // Two dead scratch dirs (attempts killed before their TempDir
        // cleanup ran) plus the published archive dir and an unrelated
        // artifact, which must survive the sweep.
        let dead_a = root.path().join(format!("{STAGING_SCRATCH_PREFIX}aaaa"));
        std::fs::create_dir_all(dead_a.join("nix/store")).unwrap();
        std::fs::write(dead_a.join("manifest.json"), b"{}").unwrap();
        let dead_b = root.path().join(format!("{STAGING_SCRATCH_PREFIX}bbbb"));
        std::fs::create_dir_all(&dead_b).unwrap();
        let published = root.path().join("archive");
        std::fs::create_dir_all(&published).unwrap();
        std::fs::write(root.path().join("fidelity.json"), b"{}").unwrap();

        sweep_staging_scratch(root.path()).unwrap();
        assert!(!dead_a.exists());
        assert!(!dead_b.exists());
        assert!(published.exists());
        assert!(root.path().join("fidelity.json").exists());

        // A missing root is fine (nothing recorded yet).
        sweep_staging_scratch(&root.path().join("nonexistent")).unwrap();
    }

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
    fn version_job_is_validated_like_every_other_job_name() {
        // Absent flag: nothing to validate.
        validate_version_job(None).unwrap();
        validate_version_job(Some("nixos.channel")).unwrap();
        // A `#` would truncate the `eval/<id>/job/<name>` recovery
        // request at the fragment; it must be rejected at the argument
        // boundary with the flag named.
        let err = validate_version_job(Some("nixos.channel#frag")).unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("--version-job"), "flag not named: {msg}");
        assert!(validate_version_job(Some("nixos..channel")).is_err());
        assert!(validate_version_job(Some("")).is_err());
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
    fn scope_job_names_are_charset_validated_at_parse_time() {
        // A bad job name must fail here, at the input boundary, in
        // milliseconds — not by mangling the politeness-budgeted
        // `eval/<id>/job/<name>` request URL in Phase 1 (a `#` truncates
        // it at the fragment) and only being rejected by the Phase-3
        // selection-expression gate after the multi-minute source prep.
        let err = parse_scope("jobs:nixpkgs.hello.x86_64-linux,nixpkgs.bad job.x86_64-linux")
            .unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("\"bad job\"") && msg.contains("characters outside"),
            "the offending component must be named; got: {msg}"
        );

        // Empty attr-path components are caught at the boundary too.
        let err = parse_scope("jobs:nixpkgs..hello").unwrap_err();
        assert!(
            err.to_string().contains("empty attr-path component"),
            "got: {err:#}"
        );

        // The constituents aggregate is an operator-supplied job name
        // that feeds a Hydra request path: same boundary, same rule.
        let err = parse_scope("constituents:tested#oops").unwrap_err();
        assert!(
            err.to_string().contains("characters outside"),
            "got: {err:#}"
        );

        // jobs-file entries go through the same validation.
        let tmp = tempfile::tempdir().unwrap();
        let f = tmp.path().join("jobs.txt");
        std::fs::write(&f, "nixpkgs.hello@x86_64-linux\n").unwrap();
        let err = parse_scope(&format!("jobs-file:{}", f.display())).unwrap_err();
        assert!(
            err.to_string().contains("characters outside"),
            "got: {err:#}"
        );
    }

    #[test]
    fn jobs_file_supports_trailing_comments() {
        // `#` starts a comment anywhere on a jobs-file line, as the
        // scope syntax documents: a trailing comment must not become
        // part of the job name.
        let tmp = tempfile::tempdir().unwrap();
        let f = tmp.path().join("jobs.txt");
        std::fs::write(
            &f,
            "# whole-line comment\n\
             nixpkgs.hello.x86_64-linux  # canary job\n\
             nixpkgs.jq.x86_64-linux#no-space\n\
             \n\
                # indented whole-line comment\n",
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
        assert_eq!(t.eval.s3_prefix, "replay");
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
    fn jobset_spec_components_are_validated_at_the_boundary() {
        // Real shapes pass — dots stay legal in jobset names.
        let (p, j) = parse_jobset_spec("nixos/trunk-combined").unwrap();
        assert_eq!((p.as_str(), j.as_str()), ("nixos", "trunk-combined"));
        let (_, j) = parse_jobset_spec("nixpkgs/release-25.05").unwrap();
        assert_eq!(j.as_str(), "release-25.05");

        // Missing separator.
        assert!(
            parse_jobset_spec("nixos")
                .unwrap_err()
                .to_string()
                .contains("<project>/<jobset>")
        );

        // A '#' would not fail the request: Url::join parses the rest
        // as a fragment that is never transmitted, so Hydra would
        // silently serve jobset `trunk` instead. The boundary names the
        // offending component instead.
        let err = parse_jobset_spec("nixos/trunk#combined").unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("jobset component") && msg.contains("trunk#combined"),
            "got: {msg}"
        );

        // '?' (query) and an extra '/' (a different Hydra path) are
        // rejected the same way, as are empty components and dot
        // traversal.
        assert!(parse_jobset_spec("nixos/trunk?official").is_err());
        assert!(parse_jobset_spec("nixos/release/25.05").is_err());
        assert!(parse_jobset_spec("/trunk").is_err());
        assert!(parse_jobset_spec("nixos/").is_err());
        assert!(parse_jobset_spec("nixos/..").is_err());
    }

    #[test]
    fn constituent_names_are_validated_at_receipt() {
        let systems = vec!["x86_64-linux".to_string()];
        let mut off_system = build("nixos.iso_minimal.aarch64-linux", Some(0), Some(1));
        off_system.system = Some("aarch64-linux".to_string());
        // Off-system AND non-embeddable: out of scope regardless of the
        // name, so it must not surface as unsupported either.
        let mut off_system_bad = build("nodePackages.\"@scope/x\".aarch64-linux", Some(0), Some(1));
        off_system_bad.system = Some("aarch64-linux".to_string());
        let mut no_system = build("nixos.systemless", Some(0), Some(1));
        no_system.system = None;
        let constituents = vec![
            // The aggregate itself is excluded (Hydra rewrites
            // aggregate derivations after evaluation).
            build("tested", Some(0), Some(1)),
            build("nixos.iso_minimal.x86_64-linux", Some(0), Some(1)),
            // A legitimate Hydra job name with a quoted attr component:
            // the recorder cannot embed it in a request path or the
            // selection expression, and an operator cannot drop it from
            // the aggregate — so it becomes an unsupported exclusion at
            // receipt instead of a deterministic Phase-3 failure after
            // the budget and source prep are spent.
            build(
                "nodePackages.\"@angular/cli\".x86_64-linux",
                Some(0),
                Some(1),
            ),
            off_system,
            off_system_bad,
            no_system,
        ];
        let resolved = resolve_constituents("tested", constituents, &systems);
        let kept: Vec<&str> = resolved.in_scope.iter().map(|b| b.job.as_str()).collect();
        assert_eq!(
            kept,
            vec!["nixos.iso_minimal.x86_64-linux", "nixos.systemless"]
        );
        assert_eq!(resolved.unsupported.len(), 1);
        let (job, why) = &resolved.unsupported[0];
        assert_eq!(job, "nodePackages.\"@angular/cli\".x86_64-linux");
        assert!(
            why.contains("characters outside"),
            "the exclusion detail must say why: {why}"
        );
    }

    #[test]
    fn vacuous_gate_error_names_both_sides_of_the_empty_join() {
        use crate::evalset::evaluator::ManifestRecord;
        use crate::evalset::fidelity::{self, FidelityVerdict};

        // The historical failure shape: the local job names carry
        // decoration (here the evaluator's display quoting) that the
        // Hydra names lack, so the join matches nothing.
        let manifest = vec![ManifestRecord {
            job: "\"nixpkgs.hello.x86_64-linux\"".into(),
            system: "x86_64-linux".into(),
            attr: "\"nixpkgs.hello.x86_64-linux\"".into(),
            drv_path: "/nix/store/7mdg60drrnh0wq1j8hmmbhll47czm107-hello-2.12.3.drv".into(),
            outputs: BTreeMap::new(),
            required_features: None,
        }];
        let truth = BTreeMap::from([(
            "nixpkgs.hello.x86_64-linux".to_string(),
            "/nix/store/7mdg60drrnh0wq1j8hmmbhll47czm107-hello-2.12.3.drv".to_string(),
        )]);
        let report = fidelity::compare_drv_paths(&manifest, &truth, fidelity::MODE_EXHAUSTIVE);
        assert!(matches!(report.verdict(), FidelityVerdict::Vacuous { .. }));

        let err = vacuous_gate_error(&report, std::path::Path::new("/out/fidelity.json"));
        let msg = format!("{err:#}");
        // Actionable: says what failed, shows one example name from each
        // side (making the format skew visible), and points at the full
        // lists.
        assert!(msg.contains("vacuous"), "got: {msg}");
        assert!(msg.contains("0 in-scope jobs were compared"), "got: {msg}");
        assert!(msg.contains("\"nixpkgs.hello.x86_64-linux\""), "got: {msg}");
        assert!(
            msg.contains("\"\\\"nixpkgs.hello.x86_64-linux\\\"\""),
            "the quote-polluted local name must be shown escaped, got: {msg}"
        );
        assert!(msg.contains("/out/fidelity.json"), "got: {msg}");

        // A side can be empty (e.g. every in-scope job failed local
        // eval): the message still reports both counts without panicking
        // on a missing example.
        let report = fidelity::compare_drv_paths(&[], &truth, fidelity::MODE_EXHAUSTIVE);
        let msg = format!(
            "{:#}",
            vacuous_gate_error(&report, std::path::Path::new("/out/fidelity.json"))
        );
        assert!(msg.contains("1 job name(s)"), "got: {msg}");
        assert!(msg.contains("0 manifest record(s)"), "got: {msg}");
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
