//! Plan stage: scope selection, warm / not-attemptable computation, the
//! plan-time validity snapshot, tenant-mode consistency checks, and the
//! archive pin recorded in campaign.json.

use std::collections::{BTreeMap, BTreeSet, HashSet};

use anyhow::{Context, Result, bail};

use crate::archive::reader::ReplayArchive;

use super::archive_input::{DepClosureEntry, ManifestEntry};
use super::glob::glob_match;
use super::grpc::StoreApi;
use super::model::now_rfc3339;
use super::report::FLAG_TENANT_UPSTREAMS_UNVERIFIED;
use super::spec::{
    ArchivePin, CampaignSpec, Filters, Mode, PLAN_COUNT_ATTEMPTABLE, PLAN_COUNT_IN_SCOPE,
    PLAN_COUNT_RSS_BEFORE, PLAN_COUNT_RSS_PEAK, PlanOutput, WARM_TENANT,
};
use super::supply::exec::{current_rss_mib, peak_rss_mib};

/// Why a job was excluded at plan time (the values of [`ScopeResult::skipped`]).
pub const SKIP_SYSTEM: &str = "system-filtered";
pub const SKIP_FEATURE: &str = "feature-excluded";
pub const SKIP_GLOB: &str = "glob-filtered";
pub const SKIP_LIMIT: &str = "limit";
pub const SKIP_JOBS_FILE: &str = "not-in-jobs-file";

/// Outcome of applying the spec's scope filters to the manifest.
#[derive(Debug, Default)]
pub struct ScopeResult {
    /// Deterministically sorted in-scope job names.
    pub in_scope: Vec<String>,
    /// job → skip reason for everything filtered out.
    pub skipped: BTreeMap<String, String>,
    /// Jobs-file entries that match no manifest job name (operator typo or a
    /// stale file), sorted. The plan stage refuses to run when non-empty.
    pub unmatched_jobs_file: Vec<String>,
}

/// Apply the spec's scope filters to the manifest. Deterministic: jobs are
/// sorted by name before the limit is applied.
///
/// Precedence: the explicit jobs-file allowlist is applied first (when
/// present it replaces the include globs entirely), then the systems
/// filter, then the excluded-features filter, then the include globs (only
/// when there is no jobs file), and finally the limit. The systems and
/// feature filters still apply to jobs named by the jobs file, so an
/// explicit list cannot resurrect a job the spec's platform filters
/// exclude.
pub fn apply_filters(
    manifest: &[ManifestEntry],
    filters: &Filters,
    jobs_file_contents: Option<&str>,
) -> ScopeResult {
    let explicit: Option<HashSet<String>> = jobs_file_contents.map(|text| {
        text.lines()
            .map(str::trim)
            .filter(|l| !l.is_empty() && !l.starts_with('#'))
            .map(String::from)
            .collect()
    });
    // Jobs-file entries that name nothing in the manifest must not vanish
    // silently: surface them so the plan stage can refuse loudly.
    let mut unmatched_jobs_file: Vec<String> = Vec::new();
    if let Some(allow) = &explicit {
        let manifest_jobs: HashSet<&str> = manifest.iter().map(|m| m.job.as_str()).collect();
        unmatched_jobs_file = allow
            .iter()
            .filter(|j| !manifest_jobs.contains(j.as_str()))
            .cloned()
            .collect();
        unmatched_jobs_file.sort();
    }
    let mut in_scope = Vec::new();
    let mut skipped = BTreeMap::new();
    let mut sorted: Vec<&ManifestEntry> = manifest.iter().collect();
    sorted.sort_by(|a, b| a.job.cmp(&b.job));

    for entry in sorted {
        if explicit
            .as_ref()
            .is_some_and(|allow| !allow.contains(&entry.job))
        {
            skipped.insert(entry.job.clone(), SKIP_JOBS_FILE.into());
            continue;
        }
        if !filters.systems.is_empty() && !filters.systems.contains(&entry.system) {
            skipped.insert(entry.job.clone(), SKIP_SYSTEM.into());
            continue;
        }
        if !filters.exclude_features.is_empty()
            && entry
                .required_features
                .iter()
                .any(|f| filters.exclude_features.contains(f))
        {
            skipped.insert(entry.job.clone(), SKIP_FEATURE.into());
            continue;
        }
        if explicit.is_none()
            && !filters.include_globs.is_empty()
            && !filters
                .include_globs
                .iter()
                .any(|g| glob_match(g, &entry.job))
        {
            skipped.insert(entry.job.clone(), SKIP_GLOB.into());
            continue;
        }
        in_scope.push(entry.job.clone());
    }
    if let Some(limit) = filters.limit {
        for job in in_scope.split_off(limit.min(in_scope.len())) {
            skipped.insert(job, SKIP_LIMIT.into());
        }
    }
    ScopeResult {
        in_scope,
        skipped,
        unmatched_jobs_file,
    }
}

/// Warm-set / not-attemptable membership computed from the dep closures of
/// the in-scope jobs.
#[derive(Debug, Default)]
pub struct WarmComputation {
    /// Union of in-scope targets' proper dependency-closure output paths.
    pub warm_set: BTreeSet<String>,
    /// In-scope jobs whose own outputs appear in the warm set.
    pub not_attemptable: BTreeSet<String>,
    /// Output path → producing dep drv (for warm-root resolution).
    pub producer: BTreeMap<String, String>,
}

/// Leaf-mode warm-set / not-attemptable computation, restricted to in-scope
/// jobs: every dependency output of an in-scope target goes in the warm set,
/// and an in-scope job whose own output sits inside another in-scope job's
/// dependency closure is not attemptable (warming it would mask the build).
///
/// Memory posture: the warm set and producer map own their Strings, sized
/// for the scoped (constituents / explicit job-list) archives the engine
/// runs today. A full-evaluation campaign needs an interning or streaming
/// pass before it is attempted.
pub fn compute_warm_sets(
    manifest: &[ManifestEntry],
    dep_closure: &[DepClosureEntry],
    in_scope: &[String],
) -> WarmComputation {
    let in_scope_set: HashSet<&str> = in_scope.iter().map(String::as_str).collect();
    let mut comp = WarmComputation::default();
    for entry in dep_closure
        .iter()
        .filter(|d| in_scope_set.contains(d.job.as_str()))
    {
        for dep in &entry.deps {
            for path in &dep.output_paths {
                // First producer wins; membership is checked before cloning so
                // re-encountered paths (shared deps) cost no allocation.
                if !comp.warm_set.contains(path) {
                    comp.warm_set.insert(path.clone());
                    comp.producer.insert(path.clone(), dep.drv_path.clone());
                }
            }
        }
    }
    for m in manifest
        .iter()
        .filter(|m| in_scope_set.contains(m.job.as_str()))
    {
        if m.outputs.values().any(|p| comp.warm_set.contains(p)) {
            comp.not_attemptable.insert(m.job.clone());
        }
    }
    comp
}

/// Plan-time validity snapshot: which in-scope target outputs are already
/// valid in rio-store before the campaign submits anything. Returns
/// (valid paths, jobs whose every output is valid).
pub async fn validity_snapshot(
    store: &dyn StoreApi,
    manifest: &[ManifestEntry],
    in_scope: &[String],
) -> Result<(BTreeSet<String>, BTreeSet<String>)> {
    let in_scope_set: HashSet<&str> = in_scope.iter().map(String::as_str).collect();
    let mut all_paths: Vec<String> = Vec::new();
    for m in manifest
        .iter()
        .filter(|m| in_scope_set.contains(m.job.as_str()))
    {
        all_paths.extend(m.outputs.values().cloned());
    }
    all_paths.sort();
    all_paths.dedup();
    let valid_map = store
        .query_valid(&all_paths)
        .await
        .context("plan-time validity snapshot")?;
    let valid: BTreeSet<String> = valid_map
        .iter()
        .filter(|(_, v)| v.is_some())
        .map(|(k, _)| k.clone())
        .collect();
    let mut cached_jobs = BTreeSet::new();
    for m in manifest
        .iter()
        .filter(|m| in_scope_set.contains(m.job.as_str()))
    {
        if !m.outputs.is_empty() && m.outputs.values().all(|p| valid.contains(p)) {
            cached_jobs.insert(m.job.clone());
        }
    }
    Ok((valid, cached_jobs))
}

/// Mode ↔ tenant-name consistency plus the launch-time upstream-set
/// assertion: the engine cannot list tenant upstreams itself (the
/// ListTenants/ListUpstreams admin RPCs are allowlisted to operator CLIs),
/// so `xtask replay launch` performs that assertion and records it in the
/// spec. Returns the low-confidence flags to carry into the report.
pub fn check_tenants(spec: &CampaignSpec, allow_unverified: bool) -> Result<Vec<String>> {
    let mut low_confidence = Vec::new();
    let expected = spec.mode.expected_build_tenant();
    if spec.tenants.build_tenant != expected {
        bail!(
            "mode {} requires build tenant '{}' but spec says '{}'",
            spec.mode.as_str(),
            expected,
            spec.tenants.build_tenant
        );
    }
    if spec.mode == Mode::Leaf && spec.tenants.warm_tenant != WARM_TENANT {
        bail!(
            "leaf mode requires warm tenant '{WARM_TENANT}', spec says '{}'",
            spec.tenants.warm_tenant
        );
    }
    if spec.mode == Mode::Leaf && spec.cluster.ssh_key_dir.is_none() {
        bail!("leaf mode requires cluster.ssh_key_dir (per-tenant gateway SSH key directory)");
    }
    if !spec.tenants.upstreams_verified {
        if !allow_unverified {
            bail!(
                "spec.tenants.upstreams_verified is false — xtask replay launch must assert the \
                 tenant upstream sets (the engine cannot: ListUpstreams is operator-CLI-only). \
                 Pass --allow-unverified-tenants to run anyway (flagged low-confidence)."
            );
        }
        low_confidence.push(FLAG_TENANT_UPSTREAMS_UNVERIFIED.to_string());
    }
    Ok(low_confidence)
}

/// Full plan-stage output (becomes the `plan` block of campaign.json plus
/// the inputs the submit queue is seeded from).
#[derive(Debug)]
pub struct PlanResult {
    pub output: PlanOutput,
    pub pin: ArchivePin,
    pub low_confidence: Vec<String>,
    /// Output path → producing drv for warm-root resolution (not persisted in
    /// campaign.json — recomputed on resume from the archive's closures).
    pub warm_producer: BTreeMap<String, String>,
}

/// Run the plan stage against an already-opened replay archive.
pub async fn run_plan(
    spec: &CampaignSpec,
    archive: &ReplayArchive,
    store: &dyn StoreApi,
    allow_unverified_tenants: bool,
) -> Result<PlanResult> {
    let archive_id = archive
        .archive_id()
        .context("the campaign engine requires a v1 archive (this archive has no archive id)")?
        .to_string();
    // A non-empty spec digest must name the archive actually opened — the
    // S3 fetch path verifies this before download, but a local --archive can
    // point anywhere, so the mismatch is caught again here.
    if !spec.archive.digest.is_empty() && spec.archive.digest != archive_id {
        bail!(
            "campaign spec pins archive digest {} but the provided archive is {archive_id}",
            spec.archive.digest
        );
    }
    let low_confidence = check_tenants(spec, allow_unverified_tenants)?;

    let manifest = super::archive_input::load_units(archive)?;
    // Plan-time closure-graph memory measurement: resident-set size before
    // the adjacency graph is loaded and the process peak after the
    // warm-set/overlap computation over it. Recorded in the plan counts so
    // the report can surface the memory cost of planning this archive.
    let rss_before_mib = current_rss_mib();
    let dep_closure = super::archive_input::load_closures(archive, &manifest)?;

    let jobs_file_contents = match &spec.filters.jobs_file {
        Some(p) => Some(
            std::fs::read_to_string(p)
                .with_context(|| format!("read jobs file {}", p.display()))?,
        ),
        None => None,
    };
    let scope = apply_filters(&manifest, &spec.filters, jobs_file_contents.as_deref());
    if !scope.unmatched_jobs_file.is_empty() {
        let shown: Vec<&str> = scope
            .unmatched_jobs_file
            .iter()
            .take(20)
            .map(String::as_str)
            .collect();
        let more = scope.unmatched_jobs_file.len() - shown.len();
        let suffix = if more > 0 {
            format!(" (+{more} more)")
        } else {
            String::new()
        };
        bail!(
            "jobs file {} lists {} job(s) not present in the archive's workload units \
             (operator typo or stale file?): {}{}",
            spec.filters
                .jobs_file
                .as_ref()
                .map(|p| p.display().to_string())
                .unwrap_or_default(),
            scope.unmatched_jobs_file.len(),
            shown.join(", "),
            suffix
        );
    }

    let warm = if spec.mode == Mode::Leaf {
        compute_warm_sets(&manifest, &dep_closure, &scope.in_scope)
    } else {
        WarmComputation::default()
    };
    let rss_peak_mib = peak_rss_mib();
    let (valid_paths, cached_jobs) = validity_snapshot(store, &manifest, &scope.in_scope).await?;

    let mut counts = BTreeMap::new();
    // The RSS measurements are best-effort (absent off-Linux or when
    // /proc is unreadable); the keys are simply omitted then.
    if let Some(mib) = rss_before_mib {
        counts.insert(
            PLAN_COUNT_RSS_BEFORE.to_string(),
            usize::try_from(mib).unwrap_or(usize::MAX),
        );
    }
    if let Some(mib) = rss_peak_mib {
        counts.insert(
            PLAN_COUNT_RSS_PEAK.to_string(),
            usize::try_from(mib).unwrap_or(usize::MAX),
        );
    }
    counts.insert(PLAN_COUNT_IN_SCOPE.to_string(), scope.in_scope.len());
    counts.insert("skipped".to_string(), scope.skipped.len());
    counts.insert("notAttemptable".to_string(), warm.not_attemptable.len());
    counts.insert(
        PLAN_COUNT_ATTEMPTABLE.to_string(),
        scope
            .in_scope
            .iter()
            .filter(|j| !warm.not_attemptable.contains(*j))
            .count(),
    );
    counts.insert("warmSet".to_string(), warm.warm_set.len());
    counts.insert("cachedPriorJobs".to_string(), cached_jobs.len());

    let output = PlanOutput {
        planned_at: now_rfc3339(),
        in_scope: scope.in_scope,
        skipped: scope.skipped,
        not_attemptable: warm.not_attemptable.iter().cloned().collect(),
        warm_set: warm.warm_set.iter().cloned().collect(),
        cached_prior_paths: valid_paths.iter().cloned().collect(),
        cached_prior_jobs: cached_jobs.iter().cloned().collect(),
        counts,
    };
    Ok(PlanResult {
        output,
        pin: ArchivePin {
            archive_id_short: crate::archive::identity::short_id(&archive_id),
            archive_id,
        },
        low_confidence,
        warm_producer: warm.producer,
    })
}

/// Build the per-job dep-drv lookup used by batch assembly and closure
/// re-attribution: job → (target drv, dep drv set). Returned as a `BTreeMap`
/// so callers iterating it (batch assembly) stay deterministic.
///
/// Memory posture: owns one String per drv path, sized for scoped eval
/// sets; a full-evaluation campaign needs interning or streaming first.
pub fn job_closures(
    dep_closure: &[DepClosureEntry],
) -> BTreeMap<String, (String, HashSet<String>)> {
    dep_closure
        .iter()
        .map(|d| {
            (
                d.job.clone(),
                (
                    d.drv_path.clone(),
                    d.deps.iter().map(|x| x.drv_path.clone()).collect(),
                ),
            )
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::run::archive_input::{load_units, write_mini_archive};
    use crate::run::grpc::test_support::FakeStoreApi;

    fn leaf_spec() -> CampaignSpec {
        let mut spec: CampaignSpec = serde_json::from_str(
            r#"{
              "mode": "leaf",
              "cluster": {"gateway_store_url": "ssh-ng://x", "ssh_key_dir": "/keys",
                          "scheduler_addr": "s:9001", "store_addr": "st:9002"},
              "tenants": {"build_tenant": "replay-leaf", "warm_tenant": "replay-warm",
                          "upstreams_verified": true}
            }"#,
        )
        .unwrap();
        spec.filters.systems = vec!["x86_64-linux".into()];
        spec.filters.exclude_features = vec!["kvm".into()];
        spec
    }

    /// Mini archive opened for a plan test: the directory guard, the open
    /// reader, and the identity returned by the fixture writer.
    fn open_mini_archive() -> (
        tempfile::TempDir,
        ReplayArchive,
        crate::run::archive_input::MiniArchive,
    ) {
        let dir = tempfile::tempdir().unwrap();
        let built = write_mini_archive(dir.path());
        let archive = ReplayArchive::open(dir.path()).unwrap();
        (dir, archive, built)
    }

    #[test]
    fn filters_apply_systems_features_globs_limit() {
        let (_dir, archive, _built) = open_mini_archive();
        let manifest = load_units(&archive).unwrap();

        let mut filters = Filters {
            systems: vec!["x86_64-linux".into()],
            exclude_features: vec!["kvm".into()],
            ..Default::default()
        };
        let scope = apply_filters(&manifest, &filters, None);
        assert_eq!(
            scope.in_scope,
            vec![
                "appA.x86_64-linux",
                "appB.x86_64-linux",
                "divergentC.x86_64-linux"
            ]
        );
        assert_eq!(scope.skipped["kvmTest.x86_64-linux"], SKIP_FEATURE);
        assert_eq!(scope.skipped["libA.aarch64-linux"], SKIP_SYSTEM);

        filters.include_globs = vec!["app*".into()];
        let scope = apply_filters(&manifest, &filters, None);
        assert_eq!(
            scope.in_scope,
            vec!["appA.x86_64-linux", "appB.x86_64-linux"]
        );
        assert_eq!(scope.skipped["divergentC.x86_64-linux"], SKIP_GLOB);

        filters.include_globs.clear();
        filters.limit = Some(1);
        let scope = apply_filters(&manifest, &filters, None);
        assert_eq!(scope.in_scope, vec!["appA.x86_64-linux"]);
        assert_eq!(scope.skipped["appB.x86_64-linux"], SKIP_LIMIT);
        assert_eq!(scope.skipped["divergentC.x86_64-linux"], SKIP_LIMIT);

        // Explicit jobs file overrides globs.
        filters.limit = None;
        let scope = apply_filters(&manifest, &filters, Some("appB.x86_64-linux\n# comment\n"));
        assert_eq!(scope.in_scope, vec!["appB.x86_64-linux"]);
        assert_eq!(scope.skipped["appA.x86_64-linux"], SKIP_JOBS_FILE);
        assert!(scope.unmatched_jobs_file.is_empty());

        // Jobs-file entries naming nothing in the workload units are surfaced.
        let scope = apply_filters(
            &manifest,
            &filters,
            Some("appB.x86_64-linux\nzzz.x86_64-linux\naaa.x86_64-linux\n"),
        );
        assert_eq!(
            scope.unmatched_jobs_file,
            vec!["aaa.x86_64-linux", "zzz.x86_64-linux"]
        );
    }

    #[test]
    fn warm_and_not_attemptable_from_dep_closure() {
        let (_dir, archive, _built) = open_mini_archive();
        let manifest = load_units(&archive).unwrap();
        let depc = crate::run::archive_input::load_closures(&archive, &manifest).unwrap();
        let in_scope = vec![
            "appA.x86_64-linux".to_string(),
            "appB.x86_64-linux".to_string(),
        ];
        let comp = compute_warm_sets(&manifest, &depc, &in_scope);
        // appB depends on libA's output and (transitively) stdenv's output →
        // both in the warm set; appA has no dependencies.
        assert_eq!(comp.warm_set.len(), 2);
        // No in-scope unit's own output sits inside another unit's closure in
        // the mini archive (libA and stdenv are dependency-only derivations).
        assert!(comp.not_attemptable.is_empty());
        // Producer map points each warm path at the drv that builds it.
        let app_b_deps = &depc
            .iter()
            .find(|d| d.job == "appB.x86_64-linux")
            .unwrap()
            .deps;
        let lib_a_dep = app_b_deps
            .iter()
            .find(|d| d.drv_path.contains("-libA-"))
            .unwrap();
        assert_eq!(
            comp.producer[&lib_a_dep.output_paths[0]],
            lib_a_dep.drv_path
        );
        // Restricting scope to a dependency-less unit → empty warm set.
        let comp2 = compute_warm_sets(&manifest, &depc, &["appA.x86_64-linux".to_string()]);
        assert!(comp2.warm_set.is_empty());
        assert!(comp2.not_attemptable.is_empty());

        // The not-attemptable rule itself: an in-scope unit whose own output
        // is inside another in-scope unit's dependency closure must be
        // excluded from the attemptable set (warming it would mask the build).
        let unit = |job: &str, drv: &str, out: &str| ManifestEntry {
            job: job.to_string(),
            system: "x86_64-linux".to_string(),
            attr: job.to_string(),
            drv_path: drv.to_string(),
            outputs: BTreeMap::from([("out".to_string(), out.to_string())]),
            required_features: Vec::new(),
        };
        let dep_drv = "/nix/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-depUnit.drv";
        let dep_out = "/nix/store/bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb-depUnit";
        let top_drv = "/nix/store/cccccccccccccccccccccccccccccccc-topUnit.drv";
        let synth_units = vec![
            unit("depUnit.x86_64-linux", dep_drv, dep_out),
            unit(
                "topUnit.x86_64-linux",
                top_drv,
                "/nix/store/dddddddddddddddddddddddddddddddd-topUnit",
            ),
        ];
        let synth_closure = vec![
            DepClosureEntry {
                job: "depUnit.x86_64-linux".to_string(),
                drv_path: dep_drv.to_string(),
                deps: vec![],
                srcs: vec![],
            },
            DepClosureEntry {
                job: "topUnit.x86_64-linux".to_string(),
                drv_path: top_drv.to_string(),
                deps: vec![crate::run::archive_input::DepDrvOutputs {
                    drv_path: dep_drv.to_string(),
                    output_paths: vec![dep_out.to_string()],
                }],
                srcs: vec![],
            },
        ];
        let in_scope = vec![
            "depUnit.x86_64-linux".to_string(),
            "topUnit.x86_64-linux".to_string(),
        ];
        let comp = compute_warm_sets(&synth_units, &synth_closure, &in_scope);
        assert_eq!(comp.warm_set.iter().collect::<Vec<_>>(), vec![dep_out]);
        assert_eq!(
            comp.not_attemptable.iter().collect::<Vec<_>>(),
            vec!["depUnit.x86_64-linux"]
        );
    }

    #[tokio::test]
    async fn plan_stage_end_to_end_with_fake_store() {
        let (_dir, archive, built) = open_mini_archive();
        let manifest = load_units(&archive).unwrap();
        // appA's output is already valid in rio-store → cached-prior.
        let app_a_out = manifest
            .iter()
            .find(|m| m.job == "appA.x86_64-linux")
            .unwrap()
            .outputs["out"]
            .clone();
        let mut store = FakeStoreApi::default();
        store.valid.insert(
            app_a_out.clone(),
            (
                crate::narhash::NarHash::parse(&"ab".repeat(32)).unwrap(),
                123,
            ),
        );

        let spec = leaf_spec();
        let result = run_plan(&spec, &archive, &store, false).await.unwrap();
        assert_eq!(result.pin.archive_id, built.archive_id);
        assert_eq!(result.pin.archive_id_short, built.archive_id_short);
        // x86_64 minus kvm: appA, appB, divergentC.
        assert_eq!(result.output.in_scope.len(), 3);
        assert_eq!(result.output.counts["attemptable"], 3);
        assert!(result.output.cached_prior_paths.contains(&app_a_out));
        assert_eq!(result.output.cached_prior_jobs, vec!["appA.x86_64-linux"]);
        assert!(result.low_confidence.is_empty());
        // The plan-time closure-graph memory measurement is recorded in the
        // counts (always available on Linux).
        assert!(result.output.counts.contains_key(PLAN_COUNT_RSS_BEFORE));
        assert!(result.output.counts[PLAN_COUNT_RSS_PEAK] > 0);
        // The validity snapshot is one batched StoreApi query, not one per job.
        assert_eq!(*store.calls.lock().unwrap(), 1);
    }

    #[tokio::test]
    async fn plan_refuses_bad_tenants_and_digest() {
        let (_dir, archive, built) = open_mini_archive();
        let store = FakeStoreApi::default();

        // Wrong tenant for the mode.
        let mut spec = leaf_spec();
        spec.tenants.build_tenant = "replay-selfhosted".into();
        let err = run_plan(&spec, &archive, &store, false).await.unwrap_err();
        assert!(err.to_string().contains("requires build tenant"), "{err}");

        // Unverified upstreams refused unless overridden.
        let mut spec = leaf_spec();
        spec.tenants.upstreams_verified = false;
        let err = run_plan(&spec, &archive, &store, false).await.unwrap_err();
        assert!(err.to_string().contains("upstreams_verified"), "{err}");
        let ok = run_plan(&spec, &archive, &store, true).await.unwrap();
        assert_eq!(ok.low_confidence, vec![FLAG_TENANT_UPSTREAMS_UNVERIFIED]);

        // A spec pinning a different archive digest than the one opened is
        // refused; pinning the right digest (or none) plans normally.
        let mut spec = leaf_spec();
        spec.archive.digest = "0".repeat(64);
        let err = run_plan(&spec, &archive, &store, false).await.unwrap_err();
        assert!(err.to_string().contains("pins archive digest"), "{err}");
        spec.archive.digest = built.archive_id.clone();
        run_plan(&spec, &archive, &store, false).await.unwrap();
    }

    #[tokio::test]
    async fn plan_refuses_jobs_file_entries_missing_from_units() {
        let (_dir, archive, _built) = open_mini_archive();
        let store = FakeStoreApi::default();
        let jobs_dir = tempfile::tempdir().unwrap();
        let jobs_file = jobs_dir.path().join("jobs.txt");
        std::fs::write(
            &jobs_file,
            "appB.x86_64-linux\nnoSuchJob.x86_64-linux\n# comment\n",
        )
        .unwrap();
        let mut spec = leaf_spec();
        spec.filters.jobs_file = Some(jobs_file.clone());

        let err = run_plan(&spec, &archive, &store, false).await.unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("noSuchJob.x86_64-linux"), "{msg}");
        assert!(msg.contains("jobs file"), "{msg}");
        assert!(
            !msg.contains("appB"),
            "matched entries must not be listed as unmatched: {msg}"
        );

        // The same file with only real jobs plans normally.
        std::fs::write(&jobs_file, "appB.x86_64-linux\n").unwrap();
        let ok = run_plan(&spec, &archive, &store, false).await.unwrap();
        assert_eq!(ok.output.in_scope, vec!["appB.x86_64-linux"]);
    }

    #[tokio::test]
    async fn plan_computes_closures_without_the_dependency_closures_capability() {
        use crate::archive::schema::{Capabilities, RequestRecord, RequestTarget, Substituters};
        use crate::archive::writer::{ArchiveWriter, ManifestSeed};

        // A minimal v1 archive with one workload unit but no closures.jsonl
        // (dependency_closures = false): the plan stage falls back to the
        // embedded ATerm walk instead of refusing the archive.
        let dir = tempfile::tempdir().unwrap();
        let drv = format!(
            "/nix/store/{}-solo-1.0.drv",
            crate::run::archive_input::fake_hash("solo-drv")
        );
        let out = format!(
            "/nix/store/{}-solo-1.0",
            crate::run::archive_input::fake_hash("solo-out")
        );
        let writer = ArchiveWriter::create(dir.path()).unwrap();
        writer
            .add_drv(
                &drv,
                &format!(
                    r#"Derive([("out","{out}","","")],[],[],"x86_64-linux","/bin/sh",["-c","true"],[("out","{out}")])"#
                ),
            )
            .unwrap();
        writer
            .write_units(&[crate::archive::schema::UnitRecord {
                drv: drv.clone(),
                label: Some("solo.x86_64-linux".to_string()),
                system: Some("x86_64-linux".to_string()),
                outputs: BTreeMap::from([("out".to_string(), out.clone())]),
                required_features: Vec::new(),
                identity_divergent: false,
            }])
            .unwrap();
        writer
            .write_requests(&[RequestRecord {
                session: 0,
                offset_s: 0.0,
                targets: vec![RequestTarget {
                    drv: drv.clone(),
                    outputs: vec!["*".to_string()],
                }],
            }])
            .unwrap();
        let stamp: jiff::Timestamp = "2026-05-28T00:00:00Z".parse().unwrap();
        writer
            .finalize(ManifestSeed {
                created_at: stamp,
                from: stamp,
                to: stamp,
                capabilities: Capabilities {
                    timed: false,
                    expected_outcomes: false,
                    output_hashes: false,
                    embedded_store_paths: false,
                    impure_env: false,
                    dependency_closures: false,
                },
                substituters: Substituters {
                    relay: vec!["https://cache.example.org".to_string()],
                    target: Vec::new(),
                },
                fat: false,
                provenance: serde_json::Map::new(),
            })
            .unwrap();
        let archive = ReplayArchive::open(dir.path()).unwrap();

        let store = FakeStoreApi::default();
        let result = run_plan(&leaf_spec(), &archive, &store, false)
            .await
            .expect("the ATerm fallback makes capability-less archives plannable");
        assert_eq!(result.output.in_scope, vec!["solo.x86_64-linux"]);
    }
}
