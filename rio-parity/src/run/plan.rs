//! Plan stage: scope selection, warm / not-attemptable computation, the
//! plan-time validity snapshot, tenant-mode consistency checks, and the
//! eval-set pin recorded in campaign.json.

use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::path::Path;

use anyhow::{Context, Result, bail};

use super::evalset_input::{DepClosureEntry, ManifestEntry};
use super::glob::glob_match;
use super::grpc::StoreApi;
use super::model::now_rfc3339;
use super::spec::{CampaignSpec, EvalSetPin, Filters, Mode, PlanOutput, WARM_TENANT};

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
/// for the scoped (constituents / explicit job-list) eval sets the engine
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
/// so `xtask parity launch` performs that assertion and records it in the
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
    if spec.mode == Mode::Leaf && spec.cluster.warm_store_url.is_none() {
        bail!("leaf mode requires cluster.warm_store_url (parity-warm ssh-ng URL)");
    }
    if !spec.tenants.upstreams_verified {
        if !allow_unverified {
            bail!(
                "spec.tenants.upstreams_verified is false — xtask parity launch must assert the \
                 tenant upstream sets (the engine cannot: ListUpstreams is operator-CLI-only). \
                 Pass --allow-unverified-tenants to run anyway (flagged low-confidence)."
            );
        }
        low_confidence.push("tenant-upstreams-unverified".to_string());
    }
    Ok(low_confidence)
}

/// Full plan-stage output (becomes the `plan` block of campaign.json plus
/// the inputs the submit queue is seeded from).
#[derive(Debug)]
pub struct PlanResult {
    pub output: PlanOutput,
    pub pin: EvalSetPin,
    pub low_confidence: Vec<String>,
    /// Output path → producing drv for warm-root resolution (not persisted in
    /// campaign.json — recomputed on resume from dep-closure.jsonl).
    pub warm_producer: BTreeMap<String, String>,
}

/// Run the plan stage against an already-downloaded eval-set directory.
pub async fn run_plan(
    spec: &CampaignSpec,
    eval_dir: &Path,
    store: &dyn StoreApi,
    allow_unverified_tenants: bool,
) -> Result<PlanResult> {
    let meta = super::evalset_input::load_meta(eval_dir)?;
    if meta.hydra_eval_id != spec.eval_set.hydra_eval_id {
        bail!(
            "eval set mismatch: spec wants hydra eval {} but evalset.json says {}",
            spec.eval_set.hydra_eval_id,
            meta.hydra_eval_id
        );
    }
    if !spec.eval_set.key_digest.is_empty() && meta.key_digest != spec.eval_set.key_digest {
        bail!(
            "eval set key-digest mismatch: spec {} vs evalset.json {}",
            spec.eval_set.key_digest,
            meta.key_digest
        );
    }
    let manifest_sha = super::evalset_input::manifest_sha256(eval_dir)?;
    if let Some(recorded) = &meta.manifest_sha256
        && recorded != &manifest_sha
    {
        bail!(
            "manifest.jsonl digest mismatch: evalset.json records {recorded}, computed {manifest_sha}"
        );
    }
    let low_confidence = check_tenants(spec, allow_unverified_tenants)?;

    let manifest = super::evalset_input::load_manifest(eval_dir)?;
    let dep_closure = super::evalset_input::load_dep_closure(eval_dir)?;
    if spec.mode == Mode::Leaf && dep_closure.is_empty() {
        bail!("leaf mode requires dep-closure.jsonl in the eval set (warm-all + not-attemptable)");
    }
    // Loud format guard: an eval set produced with a pre-adjacency flat
    // format (`depOutputPaths` only, no `deps`) would otherwise yield an empty
    // warm set and a silently-wrong leaf campaign.
    if spec.mode == Mode::Leaf
        && dep_closure
            .iter()
            .any(|d| d.deps.is_empty() && !d.legacy_dep_output_paths.is_empty())
    {
        bail!(
            "eval set was produced with an incompatible dep-closure format (flat depOutputPaths \
             without the deps adjacency); re-run rio-parity eval with a version that emits \
             {{\"deps\":[{{\"drvPath\",\"outputPaths\"}}]}} records"
        );
    }

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
            "jobs file {} lists {} job(s) not present in the eval-set manifest \
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
    let (valid_paths, cached_jobs) = validity_snapshot(store, &manifest, &scope.in_scope).await?;

    let mut counts = BTreeMap::new();
    counts.insert("inScope".to_string(), scope.in_scope.len());
    counts.insert("skipped".to_string(), scope.skipped.len());
    counts.insert("notAttemptable".to_string(), warm.not_attemptable.len());
    counts.insert(
        "attemptable".to_string(),
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
        pin: EvalSetPin {
            hydra_eval_id: meta.hydra_eval_id,
            key_digest: meta.key_digest,
            manifest_sha256: manifest_sha,
        },
        low_confidence,
        warm_producer: warm.producer,
    })
}

/// Resume gate: refuse to continue (or report) when the eval set on disk no
/// longer hashes to the manifest digest recorded in campaign.json — one
/// campaign must never mix two eval sets.
pub fn verify_manifest_digest(eval_dir: &Path, recorded: &str) -> Result<()> {
    let computed = super::evalset_input::manifest_sha256(eval_dir)?;
    if computed != recorded {
        bail!(
            "manifest digest mismatch on resume: campaign.json records {recorded} but the eval set \
             on disk hashes to {computed} — refusing to mix eval sets in one campaign"
        );
    }
    Ok(())
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
    use crate::run::evalset_input::test_fixtures::write_mini_eval_set;
    use crate::run::grpc::test_support::FakeStoreApi;

    fn leaf_spec() -> CampaignSpec {
        let mut spec: CampaignSpec = serde_json::from_str(
            r#"{
              "mode": "leaf",
              "eval_set": {"hydra_eval_id": 1824219, "key_digest": "deadbeef"},
              "cluster": {"gateway_store_url": "ssh-ng://x", "warm_store_url": "ssh-ng://w",
                          "scheduler_addr": "s:9001", "store_addr": "st:9002"},
              "tenants": {"build_tenant": "parity-leaf", "warm_tenant": "parity-warm",
                          "upstreams_verified": true}
            }"#,
        )
        .unwrap();
        spec.filters.systems = vec!["x86_64-linux".into()];
        spec.filters.exclude_features = vec!["kvm".into()];
        spec
    }

    #[test]
    fn filters_apply_systems_features_globs_limit() {
        let dir = tempfile::tempdir().unwrap();
        write_mini_eval_set(dir.path());
        let manifest = crate::run::evalset_input::load_manifest(dir.path()).unwrap();

        let mut filters = Filters {
            systems: vec!["x86_64-linux".into()],
            exclude_features: vec!["kvm".into()],
            ..Default::default()
        };
        let scope = apply_filters(&manifest, &filters, None);
        assert_eq!(
            scope.in_scope,
            vec!["appB.x86_64-linux", "libA.x86_64-linux"]
        );
        assert_eq!(scope.skipped["kvmTest.x86_64-linux"], SKIP_FEATURE);
        assert_eq!(scope.skipped["libA.aarch64-linux"], SKIP_SYSTEM);

        filters.include_globs = vec!["app*".into()];
        let scope = apply_filters(&manifest, &filters, None);
        assert_eq!(scope.in_scope, vec!["appB.x86_64-linux"]);
        assert_eq!(scope.skipped["libA.x86_64-linux"], SKIP_GLOB);

        filters.include_globs.clear();
        filters.limit = Some(1);
        let scope = apply_filters(&manifest, &filters, None);
        assert_eq!(scope.in_scope, vec!["appB.x86_64-linux"]);
        assert_eq!(scope.skipped["libA.x86_64-linux"], SKIP_LIMIT);

        // Explicit jobs file overrides globs.
        filters.limit = None;
        let scope = apply_filters(&manifest, &filters, Some("libA.x86_64-linux\n# comment\n"));
        assert_eq!(scope.in_scope, vec!["libA.x86_64-linux"]);
        assert_eq!(scope.skipped["appB.x86_64-linux"], SKIP_JOBS_FILE);
        assert!(scope.unmatched_jobs_file.is_empty());

        // Jobs-file entries naming nothing in the manifest are surfaced.
        let scope = apply_filters(
            &manifest,
            &filters,
            Some("libA.x86_64-linux\nzzz.x86_64-linux\naaa.x86_64-linux\n"),
        );
        assert_eq!(
            scope.unmatched_jobs_file,
            vec!["aaa.x86_64-linux", "zzz.x86_64-linux"]
        );
    }

    #[test]
    fn warm_and_not_attemptable_from_dep_closure() {
        let dir = tempfile::tempdir().unwrap();
        write_mini_eval_set(dir.path());
        let manifest = crate::run::evalset_input::load_manifest(dir.path()).unwrap();
        let depc = crate::run::evalset_input::load_dep_closure(dir.path()).unwrap();
        let in_scope = vec![
            "appB.x86_64-linux".to_string(),
            "libA.x86_64-linux".to_string(),
        ];
        let comp = compute_warm_sets(&manifest, &depc, &in_scope);
        // appB depends on libA's output and stdenv's output → both in the warm set.
        assert_eq!(comp.warm_set.len(), 2);
        // libA's own output is inside appB's closure → libA is not-attemptable.
        assert_eq!(
            comp.not_attemptable.iter().collect::<Vec<_>>(),
            vec!["libA.x86_64-linux"]
        );
        // Producer map points the libA output at the libA drv.
        let lib_a = manifest
            .iter()
            .find(|m| m.job == "libA.x86_64-linux")
            .unwrap();
        let lib_a_out = lib_a.outputs["out"].clone();
        assert_eq!(comp.producer[&lib_a_out], lib_a.drv_path);
        // Restricting scope to libA only → empty warm set (no deps).
        let comp2 = compute_warm_sets(&manifest, &depc, &["libA.x86_64-linux".to_string()]);
        assert!(comp2.warm_set.is_empty());
        assert!(comp2.not_attemptable.is_empty());
    }

    #[tokio::test]
    async fn plan_stage_end_to_end_with_fake_store() {
        let dir = tempfile::tempdir().unwrap();
        write_mini_eval_set(dir.path());
        let manifest = crate::run::evalset_input::load_manifest(dir.path()).unwrap();
        // libA's output is already valid in rio-store → cached-prior.
        let lib_a_out = manifest
            .iter()
            .find(|m| m.job == "libA.x86_64-linux")
            .unwrap()
            .outputs["out"]
            .clone();
        let mut store = FakeStoreApi::default();
        store
            .valid
            .insert(lib_a_out.clone(), ("ab".repeat(32), 123));

        let spec = leaf_spec();
        let result = run_plan(&spec, dir.path(), &store, false).await.unwrap();
        assert_eq!(result.pin.hydra_eval_id, 1824219);
        assert_eq!(result.pin.manifest_sha256.len(), 64);
        assert_eq!(result.output.in_scope.len(), 2);
        assert_eq!(result.output.counts["attemptable"], 1); // appB only
        assert!(result.output.cached_prior_paths.contains(&lib_a_out));
        assert_eq!(result.output.cached_prior_jobs, vec!["libA.x86_64-linux"]);
        assert!(result.low_confidence.is_empty());
        // The validity snapshot is one batched StoreApi query, not one per job.
        assert_eq!(*store.calls.lock().unwrap(), 1);
    }

    #[tokio::test]
    async fn plan_refuses_bad_tenants_and_digest() {
        let dir = tempfile::tempdir().unwrap();
        write_mini_eval_set(dir.path());
        let store = FakeStoreApi::default();

        // Wrong tenant for the mode.
        let mut spec = leaf_spec();
        spec.tenants.build_tenant = "parity-selfhosted".into();
        let err = run_plan(&spec, dir.path(), &store, false)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("requires build tenant"), "{err}");

        // Unverified upstreams refused unless overridden.
        let mut spec = leaf_spec();
        spec.tenants.upstreams_verified = false;
        let err = run_plan(&spec, dir.path(), &store, false)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("upstreams_verified"), "{err}");
        let ok = run_plan(&spec, dir.path(), &store, true).await.unwrap();
        assert_eq!(ok.low_confidence, vec!["tenant-upstreams-unverified"]);

        // Resume digest gate.
        let good = crate::run::evalset_input::manifest_sha256(dir.path()).unwrap();
        verify_manifest_digest(dir.path(), &good).unwrap();
        assert!(verify_manifest_digest(dir.path(), "0000").is_err());

        // Wrong eval id.
        let mut spec = leaf_spec();
        spec.eval_set.hydra_eval_id = 1;
        let err = run_plan(&spec, dir.path(), &store, false)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("eval set mismatch"), "{err}");
    }

    #[tokio::test]
    async fn plan_refuses_jobs_file_entries_missing_from_manifest() {
        let dir = tempfile::tempdir().unwrap();
        write_mini_eval_set(dir.path());
        let store = FakeStoreApi::default();
        let jobs_file = dir.path().join("jobs.txt");
        std::fs::write(
            &jobs_file,
            "appB.x86_64-linux\nnoSuchJob.x86_64-linux\n# comment\n",
        )
        .unwrap();
        let mut spec = leaf_spec();
        spec.filters.jobs_file = Some(jobs_file.clone());

        let err = run_plan(&spec, dir.path(), &store, false)
            .await
            .unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("noSuchJob.x86_64-linux"), "{msg}");
        assert!(msg.contains("jobs file"), "{msg}");
        assert!(
            !msg.contains("appB"),
            "matched entries must not be listed as unmatched: {msg}"
        );

        // The same file with only real jobs plans normally.
        std::fs::write(&jobs_file, "appB.x86_64-linux\n").unwrap();
        let ok = run_plan(&spec, dir.path(), &store, false).await.unwrap();
        assert_eq!(ok.output.in_scope, vec!["appB.x86_64-linux"]);
    }

    #[tokio::test]
    async fn plan_refuses_recorded_digest_key_digest_and_missing_dep_closure() {
        let store = FakeStoreApi::default();

        // evalset.json records a manifest_sha256 that doesn't match the bytes
        // on disk.
        let dir = tempfile::tempdir().unwrap();
        write_mini_eval_set(dir.path());
        let meta_path = dir.path().join("evalset.json");
        let mut meta: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&meta_path).unwrap()).unwrap();
        meta["manifest_sha256"] = serde_json::Value::String("0".repeat(64));
        std::fs::write(&meta_path, serde_json::to_vec_pretty(&meta).unwrap()).unwrap();
        let err = run_plan(&leaf_spec(), dir.path(), &store, false)
            .await
            .unwrap_err();
        assert!(
            err.to_string().contains("manifest.jsonl digest mismatch"),
            "{err}"
        );

        // Spec pins a different eval-set key digest than evalset.json records.
        let dir = tempfile::tempdir().unwrap();
        write_mini_eval_set(dir.path());
        let mut spec = leaf_spec();
        spec.eval_set.key_digest = "feedface".into();
        let err = run_plan(&spec, dir.path(), &store, false)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("key-digest mismatch"), "{err}");

        // Leaf mode without dep-closure.jsonl cannot compute the warm set.
        let dir = tempfile::tempdir().unwrap();
        write_mini_eval_set(dir.path());
        std::fs::remove_file(dir.path().join("dep-closure.jsonl")).unwrap();
        let err = run_plan(&leaf_spec(), dir.path(), &store, false)
            .await
            .unwrap_err();
        assert!(
            err.to_string().contains("requires dep-closure.jsonl"),
            "{err}"
        );
    }

    #[tokio::test]
    async fn plan_refuses_legacy_flat_dep_closure_format() {
        use std::io::Write;
        let dir = tempfile::tempdir().unwrap();
        write_mini_eval_set(dir.path());
        // Overwrite dep-closure.jsonl with a legacy flat format
        // (depOutputPaths, no deps adjacency): the leaf plan stage must fail
        // loudly instead of computing an empty warm set.
        let mut f = std::fs::File::create(dir.path().join("dep-closure.jsonl")).unwrap();
        writeln!(
            f,
            r#"{{"job":"appB.x86_64-linux","drvPath":"/nix/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-appB.drv","depOutputPaths":["/nix/store/bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb-libA"],"caOutputs":[]}}"#
        )
        .unwrap();
        let store = FakeStoreApi::default();
        let err = run_plan(&leaf_spec(), dir.path(), &store, false)
            .await
            .unwrap_err();
        assert!(
            err.to_string().contains("incompatible dep-closure format"),
            "{err}"
        );
    }
}
