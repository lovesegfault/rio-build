//! Adapters from an open replay archive to the engine's plan inputs
//! (units, per-unit dependency closures, exclusions), plus the synthetic
//! mini-archive used by the engine's offline tests.
//!
//! The plan stage keeps consuming the same in-memory shapes it used for
//! the legacy eval-set artifacts ([`ManifestEntry`], [`DepClosureEntry`]);
//! these adapters fill those shapes from an open
//! [`ReplayArchive`] instead:
//! the requests-derived workload becomes manifest entries (enriched from
//! `units.jsonl` records where the recorder wrote them, recovered from the
//! embedded derivation ATerms otherwise), the per-derivation adjacency in
//! `closures.jsonl` is expanded into per-unit transitive dependency
//! closures, and `exclusions.jsonl` feeds the plan-time completeness
//! accounting.

use std::collections::{BTreeMap, BTreeSet, HashMap, VecDeque};

use anyhow::{Context as _, Result};
use rio_nix::derivation::Derivation;
use serde::{Deserialize, Serialize};

use crate::archive::reader::ReplayArchive;
use crate::archive::schema::{Capability, ClosureRecord, UnitRecord};

/// One workload unit in the shape the plan stage consumes: job name,
/// system, derivation, declared outputs, and required features. Field
/// names keep the camelCase wire form the legacy `manifest.jsonl` reader
/// used, so downstream serializations of these entries are unchanged.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ManifestEntry {
    pub job: String,
    pub system: String,
    #[serde(default)]
    pub attr: String,
    #[serde(rename = "drvPath")]
    pub drv_path: String,
    /// output name → store path
    #[serde(default)]
    pub outputs: BTreeMap<String, String>,
    #[serde(rename = "requiredFeatures", default)]
    pub required_features: Vec<String>,
}

/// One workload unit's proper transitive dependency closure in adjacency
/// form: each dependency derivation with its declared output paths.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DepClosureEntry {
    pub job: String,
    #[serde(rename = "drvPath")]
    pub drv_path: String,
    #[serde(default)]
    pub deps: Vec<DepDrvOutputs>,
}

/// One dependency derivation and the output paths it declares.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DepDrvOutputs {
    #[serde(rename = "drvPath")]
    pub drv_path: String,
    #[serde(rename = "outputPaths", default)]
    pub output_paths: Vec<String>,
}

/// Workload units in the shape the plan stage consumes, one
/// [`ManifestEntry`] per derivation in the requests-derived workload.
///
/// The required `requests.jsonl` member is the source of the workload (the
/// union of all request targets, per the archive contract); the optional
/// `units.jsonl` member only enriches entries. A workload derivation with
/// a unit record takes `job`/`attr` from the record's label (drv basename
/// without the `.drv` suffix when the label is absent), `system` (empty
/// string when absent), declared outputs, and required features. A
/// workload derivation without one has the same fields recovered from its
/// embedded derivation ATerm (see `workload_entry`) — so an archive
/// without `units.jsonl` (or with one covering only part of the workload)
/// still plans its full workload instead of silently shrinking it.
///
/// The returned entries are sorted by job then drv path so plan-time
/// iteration order is deterministic.
pub fn load_units(archive: &ReplayArchive) -> Result<Vec<ManifestEntry>> {
    let units = archive.units();
    let mut entries = Vec::with_capacity(archive.workload_units().len());
    for drv in archive.workload_units() {
        entries.push(workload_entry(archive, drv, units.get(drv))?);
    }
    entries.sort_by(|a, b| a.job.cmp(&b.job).then_with(|| a.drv_path.cmp(&b.drv_path)));
    Ok(entries)
}

/// One workload unit's manifest entry: metadata from its `units.jsonl`
/// record when the recorder wrote one, recovered from the unit's embedded
/// derivation ATerm otherwise.
///
/// The recovery parses the real derivation rather than synthesizing an
/// entry from the request target alone: the entry's `outputs` feed the
/// warm-set computation, cached-prior detection, and NAR comparison, so an
/// empty-outputs placeholder would not fail — it would silently degrade
/// exactly those comparisons for the synthesized units. A workload
/// derivation whose ATerm is missing from the archive is therefore a hard
/// per-unit error (the archive cannot say what the unit produces), never a
/// silent skip.
fn workload_entry(
    archive: &ReplayArchive,
    drv: &str,
    record: Option<&UnitRecord>,
) -> Result<ManifestEntry> {
    if let Some(record) = record {
        let job = unit_job(record);
        return Ok(ManifestEntry {
            job: job.clone(),
            system: record.system.clone().unwrap_or_default(),
            attr: job,
            drv_path: record.drv.clone(),
            outputs: record.outputs.clone(),
            required_features: record.required_features.clone(),
        });
    }
    let text = archive.read_drv(drv).with_context(|| {
        format!(
            "workload unit {drv} has no units.jsonl record and no readable derivation in the \
             archive to recover its outputs from"
        )
    })?;
    let derivation = Derivation::parse(&text)
        .with_context(|| format!("parsing the embedded derivation of workload unit {drv}"))?;
    // Statically declared output paths only, mirroring what recorders put
    // in units.jsonl: floating content-addressed outputs have no path until
    // they are built, so they cannot enter path-keyed planning.
    let outputs: BTreeMap<String, String> = derivation
        .outputs()
        .iter()
        .filter(|output| !output.path().is_empty())
        .map(|output| (output.name().to_string(), output.path().to_string()))
        .collect();
    // `requiredSystemFeatures` is an ASCII-whitespace-separated env list,
    // kept in declaration order — the same extraction the recorder applies
    // when it backfills unit metadata.
    let required_features: Vec<String> = derivation
        .env()
        .get("requiredSystemFeatures")
        .map(|raw| raw.split_ascii_whitespace().map(String::from).collect())
        .unwrap_or_default();
    let job = drv_basename_job(drv);
    Ok(ManifestEntry {
        job: job.clone(),
        system: derivation.platform().to_string(),
        attr: job,
        drv_path: drv.to_string(),
        outputs,
        required_features,
    })
}

/// Per-unit proper transitive dependency closures: for each unit, walk the
/// direct-adjacency `inputs` edges breadth-first and emit every reachable
/// derivation (the unit itself excluded) with its statically declared
/// output paths (floating content-addressed outputs are skipped).
///
/// The adjacency comes from `closures.jsonl` when the archive declares the
/// `dependency_closures` capability; otherwise it is recovered by walking
/// and parsing the embedded derivation ATerms — the documented fallback
/// for recorders that skip the ATerm pass, and the only construction v0
/// archives (which never carry `closures.jsonl`) can use. A derivation the
/// fallback walk needs but the archive does not embed is a hard error
/// naming it and the unit that needed it.
pub fn load_closures(
    archive: &ReplayArchive,
    units: &[ManifestEntry],
) -> Result<Vec<DepClosureEntry>> {
    let recovered: Vec<ClosureRecord>;
    let records: &[ClosureRecord] =
        if Capability::DependencyClosures.enabled_in(archive.capabilities()) {
            archive.closures()
        } else {
            let roots: Vec<String> = units.iter().map(|unit| unit.drv_path.clone()).collect();
            recovered = aterm_adjacency(archive, &roots)?;
            &recovered
        };
    let adjacency: HashMap<&str, &ClosureRecord> = records
        .iter()
        .map(|record| (record.drv.as_str(), record))
        .collect();

    let mut entries = Vec::with_capacity(units.len());
    for unit in units {
        // Breadth-first walk over direct inputs; the visited set keeps a
        // cyclic or duplicated edge from looping, and the unit itself never
        // counts as its own dependency.
        let mut deps: BTreeSet<&str> = BTreeSet::new();
        let mut queue: VecDeque<&str> = adjacency
            .get(unit.drv_path.as_str())
            .map(|record| record.inputs.iter().map(String::as_str).collect())
            .unwrap_or_default();
        while let Some(drv) = queue.pop_front() {
            if drv == unit.drv_path {
                continue;
            }
            if !deps.insert(drv) {
                continue;
            }
            if let Some(record) = adjacency.get(drv) {
                for input in &record.inputs {
                    if !deps.contains(input.as_str()) {
                        queue.push_back(input);
                    }
                }
            }
        }
        let deps = deps
            .into_iter()
            .map(|drv| DepDrvOutputs {
                drv_path: drv.to_string(),
                // A dependency without an adjacency record contributes no
                // declared outputs; it still appears so closure-overlap
                // accounting sees the derivation.
                output_paths: adjacency
                    .get(drv)
                    .map(|record| record.outputs.values().filter_map(Clone::clone).collect())
                    .unwrap_or_default(),
            })
            .collect();
        entries.push(DepClosureEntry {
            job: unit.job.clone(),
            drv_path: unit.drv_path.clone(),
            deps,
        });
    }
    Ok(entries)
}

/// Direct-adjacency records recovered from the embedded derivation ATerms:
/// the union closure of `roots`, each derivation mapped into the
/// `closures.jsonl` record shape (direct inputs, input sources, declared
/// outputs — floating content-addressed outputs as `None`). Reuses the
/// supply planner's ATerm closure walk so both capability-less consumers
/// construct the same closure.
fn aterm_adjacency(archive: &ReplayArchive, roots: &[String]) -> Result<Vec<ClosureRecord>> {
    let closure = super::supply::closure_from_drv_texts(archive, roots)?;
    Ok(closure
        .topo
        .into_iter()
        .map(|node| ClosureRecord {
            drv: node.drv_path,
            inputs: node.input_drvs.into_keys().collect(),
            srcs: node.input_srcs,
            outputs: node
                .outputs
                .into_iter()
                .map(|(name, path)| {
                    let path = (!path.is_empty()).then_some(path);
                    (name, path)
                })
                .collect(),
        })
        .collect())
}

/// Count of `exclusions.jsonl` records per exclusion reason (empty when
/// the archive carries no exclusions member).
pub fn exclusion_counts(archive: &ReplayArchive) -> BTreeMap<String, usize> {
    let mut counts: BTreeMap<String, usize> = BTreeMap::new();
    for record in archive.exclusions() {
        *counts.entry(record.reason.clone()).or_insert(0) += 1;
    }
    counts
}

/// Total recorder-side exclusion records, or `None` when the archive
/// carries no exclusions member at all. The distinction matters for the
/// comparability accounting: an absent member means the recorder declared
/// nothing about exclusions (no completeness penalty applies), while a
/// present-but-empty member is a positive "nothing was excluded" claim.
pub fn exclusions_recorded(archive: &ReplayArchive) -> Option<usize> {
    archive
        .manifest()
        .files
        .contains_key(crate::archive::EXCLUSIONS_MEMBER)
        .then(|| archive.exclusions().len())
}

/// Job names (label, or the drv-basename fallback) of every unit the
/// recorder marked identity divergent, sorted.
pub fn identity_divergent_units(archive: &ReplayArchive) -> Result<Vec<String>> {
    let mut divergent: Vec<String> = archive
        .units()
        .values()
        .filter(|record| record.identity_divergent)
        .map(unit_job)
        .collect();
    divergent.sort();
    Ok(divergent)
}

/// The job name of a unit record: its label, falling back to the drv
/// basename without the `.drv` suffix when the recorder wrote no label.
fn unit_job(record: &UnitRecord) -> String {
    match &record.label {
        Some(label) => label.clone(),
        None => drv_basename_job(&record.drv),
    }
}

/// The label-less job-name fallback: the drv basename without the `.drv`
/// suffix. Shared by record-carrying units without a label and workload
/// units recovered straight from their ATerm (which has no label at all).
fn drv_basename_job(drv: &str) -> String {
    let base = drv.rsplit('/').next().unwrap_or(drv);
    base.strip_suffix(".drv").unwrap_or(base).to_string()
}

/// Identity of the synthetic archive written by [`write_mini_archive`].
#[cfg(test)]
#[derive(Debug)]
pub(crate) struct MiniArchive {
    pub archive_id: String,
    pub archive_id_short: String,
}

/// Deterministic, name-distinct, nixbase32-valid 32-char hash part so
/// every synthetic store path has a unique hash part (narinfo lookups key
/// on it).
#[cfg(test)]
pub(crate) fn fake_hash(seed: &str) -> String {
    const CHARS: &[u8] = b"0123456789abcdfghijklmnpqrsvwxyz";
    let bytes = seed.as_bytes();
    (0..32)
        .map(|i| CHARS[(bytes[i % bytes.len()] as usize + i) % CHARS.len()] as char)
        .collect()
}

/// Synthetic `Derive(...)` ATerm text for one fixture derivation: the
/// given outputs, a dependency on the `out` output of each input
/// derivation, no input sources, and a trivial builder.
#[cfg(test)]
fn synth_aterm(outputs: &[(&str, &str)], input_drvs: &[&str], system: &str) -> String {
    synth_aterm_with_env(outputs, input_drvs, system, &[])
}

/// [`synth_aterm`] plus extra env entries (e.g. `requiredSystemFeatures`),
/// appended after the per-output env entries in the given order.
#[cfg(test)]
fn synth_aterm_with_env(
    outputs: &[(&str, &str)],
    input_drvs: &[&str],
    system: &str,
    extra_env: &[(&str, &str)],
) -> String {
    let outs = outputs
        .iter()
        .map(|(name, path)| format!(r#"("{name}","{path}","","")"#))
        .collect::<Vec<_>>()
        .join(",");
    let inputs = input_drvs
        .iter()
        .map(|drv| format!(r#"("{drv}",["out"])"#))
        .collect::<Vec<_>>()
        .join(",");
    let env = outputs
        .iter()
        .chain(extra_env)
        .map(|(name, path)| format!(r#"("{name}","{path}")"#))
        .collect::<Vec<_>>()
        .join(",");
    format!(r#"Derive([{outs}],[{inputs}],[],"{system}","/bin/sh",["-c","true"],[{env}])"#) + "\n"
}

/// Write a tiny synthetic directory-form v1 replay archive into `dir`,
/// shaped like the eval recorder's output: five workload units
/// (`appA.x86_64-linux` built with per-output hashes, `appB.x86_64-linux`
/// with `out`+`dev` outputs, `divergentC.x86_64-linux` identity divergent
/// with an `unknown` outcome, `kvmTest.x86_64-linux` requiring the `kvm`
/// feature, `libA.aarch64-linux` on aarch64), two dependency-only
/// derivations forming the chain appB → libA → stdenv in `closures.jsonl`,
/// one `eval-error` exclusion, and synthetic ATerm members for all seven
/// derivations.
#[cfg(test)]
pub(crate) fn write_mini_archive(dir: &std::path::Path) -> MiniArchive {
    use crate::archive::schema::{
        Capabilities, EXCLUSION_REASON_EVAL_ERROR, ExclusionRecord, ExpectedOutcome, OutcomeRecord,
        OutputHash, RequestRecord, RequestTarget, Substituters,
    };
    use crate::archive::writer::{ArchiveWriter, ManifestSeed};

    let drv = |name: &str| {
        format!(
            "/nix/store/{}-{name}.drv",
            fake_hash(&format!("{name}-drv"))
        )
    };
    let out = |name: &str| format!("/nix/store/{}-{name}", fake_hash(&format!("{name}-out")));

    // Store paths: the five workload units, plus the two dependency-only
    // derivations appB reaches (libA directly, stdenv through libA). The
    // versioned names keep a `-libA-` / `-stdenv-` segment in the store
    // paths, the same shape real nixpkgs derivations have.
    let app_a_drv = drv("appA-1.0");
    let app_a_out = out("appA-1.0");
    let app_b_drv = drv("appB-1.0");
    let app_b_out = out("appB-1.0");
    let app_b_dev = out("appB-1.0-dev");
    let divergent_c_drv = drv("divergentC-1.0");
    let divergent_c_out = out("divergentC-1.0");
    let kvm_test_drv = drv("kvmTest-1.0");
    let kvm_test_out = out("kvmTest-1.0");
    let lib_a_arm_drv = drv("libA-arm");
    let lib_a_arm_out = out("libA-arm");
    let lib_a_drv = drv("libA-1.0");
    let lib_a_out = out("libA-1.0");
    let stdenv_drv = drv("stdenv-2.0");
    let stdenv_out = out("stdenv-2.0");

    let writer = ArchiveWriter::create(dir).unwrap();

    // nix/store/*.drv — synthetic ATerms for all seven derivations. The
    // input edges mirror the closures.jsonl adjacency below so the
    // writer's drv-closure completeness walk covers the same chain.
    writer
        .add_drv(
            &app_a_drv,
            &synth_aterm(&[("out", app_a_out.as_str())], &[], "x86_64-linux"),
        )
        .unwrap();
    writer
        .add_drv(
            &app_b_drv,
            &synth_aterm(
                &[("dev", app_b_dev.as_str()), ("out", app_b_out.as_str())],
                &[lib_a_drv.as_str()],
                "x86_64-linux",
            ),
        )
        .unwrap();
    writer
        .add_drv(
            &divergent_c_drv,
            &synth_aterm(&[("out", divergent_c_out.as_str())], &[], "x86_64-linux"),
        )
        .unwrap();
    writer
        .add_drv(
            &kvm_test_drv,
            &synth_aterm(&[("out", kvm_test_out.as_str())], &[], "x86_64-linux"),
        )
        .unwrap();
    writer
        .add_drv(
            &lib_a_arm_drv,
            &synth_aterm(&[("out", lib_a_arm_out.as_str())], &[], "aarch64-linux"),
        )
        .unwrap();
    writer
        .add_drv(
            &lib_a_drv,
            &synth_aterm(
                &[("out", lib_a_out.as_str())],
                &[stdenv_drv.as_str()],
                "x86_64-linux",
            ),
        )
        .unwrap();
    writer
        .add_drv(
            &stdenv_drv,
            &synth_aterm(&[("out", stdenv_out.as_str())], &[], "x86_64-linux"),
        )
        .unwrap();

    // units.jsonl — the five workload units, with the filter-relevant
    // variety the engine's offline tests rely on (multi-output, required
    // feature, non-x86 system, identity divergence).
    writer
        .write_units(&[
            UnitRecord {
                drv: app_a_drv.clone(),
                label: Some("appA.x86_64-linux".to_string()),
                system: Some("x86_64-linux".to_string()),
                outputs: BTreeMap::from([("out".to_string(), app_a_out.clone())]),
                required_features: Vec::new(),
                identity_divergent: false,
            },
            UnitRecord {
                drv: app_b_drv.clone(),
                label: Some("appB.x86_64-linux".to_string()),
                system: Some("x86_64-linux".to_string()),
                outputs: BTreeMap::from([
                    ("out".to_string(), app_b_out.clone()),
                    ("dev".to_string(), app_b_dev.clone()),
                ]),
                required_features: Vec::new(),
                identity_divergent: false,
            },
            UnitRecord {
                drv: divergent_c_drv.clone(),
                label: Some("divergentC.x86_64-linux".to_string()),
                system: Some("x86_64-linux".to_string()),
                outputs: BTreeMap::from([("out".to_string(), divergent_c_out.clone())]),
                required_features: Vec::new(),
                identity_divergent: true,
            },
            UnitRecord {
                drv: kvm_test_drv.clone(),
                label: Some("kvmTest.x86_64-linux".to_string()),
                system: Some("x86_64-linux".to_string()),
                outputs: BTreeMap::from([("out".to_string(), kvm_test_out.clone())]),
                required_features: vec!["kvm".to_string()],
                identity_divergent: false,
            },
            UnitRecord {
                drv: lib_a_arm_drv.clone(),
                label: Some("libA.aarch64-linux".to_string()),
                system: Some("aarch64-linux".to_string()),
                outputs: BTreeMap::from([("out".to_string(), lib_a_arm_out.clone())]),
                required_features: Vec::new(),
                identity_divergent: false,
            },
        ])
        .unwrap();

    // requests.jsonl — the recorder is timeless: one synthesized request
    // per workload unit, all in session 0, asking for every output.
    let requests: Vec<RequestRecord> = [
        &app_a_drv,
        &app_b_drv,
        &divergent_c_drv,
        &kvm_test_drv,
        &lib_a_arm_drv,
    ]
    .into_iter()
    .map(|unit_drv| RequestRecord {
        session: 0,
        offset_s: 0.0,
        targets: vec![RequestTarget {
            drv: unit_drv.clone(),
            outputs: vec!["*".to_string()],
        }],
    })
    .collect();
    writer.write_requests(&requests).unwrap();

    // outcomes.jsonl — recorder records are session-less. appA is built
    // with a per-output NAR identity, appB is built without hashes,
    // divergentC is unknown; kvmTest and the aarch64 unit carry no record.
    writer
        .write_outcomes(&[
            OutcomeRecord {
                session: None,
                drv: app_a_drv.clone(),
                outcome: ExpectedOutcome::Built,
                detail: None,
                duration_s: None,
                stop_offset_s: None,
                outputs: BTreeMap::from([(
                    "out".to_string(),
                    OutputHash {
                        nar_hash: crate::narhash::NarHash::parse(&"ab".repeat(32)).unwrap(),
                        nar_size: 226_504,
                    },
                )]),
            },
            OutcomeRecord {
                session: None,
                drv: app_b_drv.clone(),
                outcome: ExpectedOutcome::Built,
                detail: None,
                duration_s: None,
                stop_offset_s: None,
                outputs: BTreeMap::new(),
            },
            OutcomeRecord {
                session: None,
                drv: divergent_c_drv.clone(),
                outcome: ExpectedOutcome::Unknown,
                detail: None,
                duration_s: None,
                stop_offset_s: None,
                outputs: BTreeMap::new(),
            },
        ])
        .unwrap();

    // closures.jsonl — direct adjacency for every derivation in the union
    // closure; appB reaches stdenv only transitively through libA.
    let closure = |drv_path: &str, inputs: Vec<String>, outputs: &[(&str, &str)]| ClosureRecord {
        drv: drv_path.to_string(),
        inputs,
        srcs: Vec::new(),
        outputs: outputs
            .iter()
            .map(|(name, path)| (name.to_string(), Some(path.to_string())))
            .collect(),
    };
    writer
        .write_closures(&[
            closure(&app_a_drv, Vec::new(), &[("out", &app_a_out)]),
            closure(
                &app_b_drv,
                vec![lib_a_drv.clone()],
                &[("dev", &app_b_dev), ("out", &app_b_out)],
            ),
            closure(&divergent_c_drv, Vec::new(), &[("out", &divergent_c_out)]),
            closure(&kvm_test_drv, Vec::new(), &[("out", &kvm_test_out)]),
            closure(&lib_a_arm_drv, Vec::new(), &[("out", &lib_a_arm_out)]),
            closure(&lib_a_drv, vec![stdenv_drv.clone()], &[("out", &lib_a_out)]),
            closure(&stdenv_drv, Vec::new(), &[("out", &stdenv_out)]),
        ])
        .unwrap();

    // exclusions.jsonl — one attribute the recorder could not evaluate.
    writer
        .write_exclusions(&[ExclusionRecord {
            label: Some("brokenPkg.x86_64-linux".to_string()),
            drv: None,
            reason: EXCLUSION_REASON_EVAL_ERROR.to_string(),
            detail: Some("attribute 'missing' not found".to_string()),
        }])
        .unwrap();

    // manifest.json — capabilities exactly as the eval recorder claims
    // them (timeless, with expected outcomes, output hashes, and
    // dependency closures; no impure env, no embedded store paths), a
    // fixed timestamp so the fixture is deterministic, and an opaque
    // provenance block.
    let stamp: jiff::Timestamp = "2026-05-28T00:00:00Z".parse().unwrap();
    let mut provenance = serde_json::Map::new();
    provenance.insert(
        "recorder".to_string(),
        serde_json::Value::from("mini-archive-fixture"),
    );
    let finalized = writer
        .finalize(ManifestSeed {
            created_at: stamp,
            from: stamp,
            to: stamp,
            capabilities: Capabilities {
                timed: false,
                expected_outcomes: true,
                output_hashes: true,
                embedded_store_paths: false,
                impure_env: false,
                dependency_closures: true,
            },
            substituters: Substituters {
                relay: vec!["https://cache.example.org".to_string()],
                target: Vec::new(),
            },
            fat: false,
            provenance,
        })
        .unwrap();
    let archive_id_short = crate::archive::identity::short_id(&finalized.archive_id);
    MiniArchive {
        archive_id: finalized.archive_id,
        archive_id_short,
    }
}

/// Write a tiny synthetic directory-form v1 TIMED replay archive into `dir`:
/// two workload units (`appA.x86_64-linux` requested at offset 0.0 s,
/// `appB.x86_64-linux` at offset 1.0 s), appB depending on one
/// dependency-only derivation (libA), expected outcomes `built` for appA
/// (1.0 s recorded duration) and — when `with_interruption` — `disconnected`
/// for appB with a recorded stop offset of 2.0 s (`built` like appA
/// otherwise), `closures.jsonl` adjacency for all three derivations, and
/// synthetic ATerm members. Capabilities: `timed`, `expected_outcomes`,
/// `dependency_closures`.
///
/// When `impure_demote_app_b` is set, an `impure-env.json` member lists an
/// impure environment variable for appB (and the manifest declares the
/// `impure_env` capability): appB then has an expected-outcome record but is
/// demoted out of the workload — combined with `with_interruption`, the
/// archive's only recorded interruption is over a unit the campaign supplies
/// instead of building.
#[cfg(test)]
pub(crate) fn write_mini_timed_archive(
    dir: &std::path::Path,
    with_interruption: bool,
    impure_demote_app_b: bool,
) -> MiniArchive {
    use crate::archive::schema::{
        Capabilities, ExpectedOutcome, ImpureEnv, OutcomeRecord, RequestRecord, RequestTarget,
        Substituters,
    };
    use crate::archive::writer::{ArchiveWriter, ManifestSeed};

    let drv = |name: &str| {
        format!(
            "/nix/store/{}-{name}.drv",
            fake_hash(&format!("{name}-drv"))
        )
    };
    let out = |name: &str| format!("/nix/store/{}-{name}", fake_hash(&format!("{name}-out")));

    let app_a_drv = drv("appA-1.0");
    let app_a_out = out("appA-1.0");
    let app_b_drv = drv("appB-1.0");
    let app_b_out = out("appB-1.0");
    let lib_a_drv = drv("libA-1.0");
    let lib_a_out = out("libA-1.0");

    let writer = ArchiveWriter::create(dir).unwrap();

    // nix/store/*.drv — synthetic ATerms; appB's input edge on libA mirrors
    // the closures.jsonl adjacency below.
    writer
        .add_drv(
            &app_a_drv,
            &synth_aterm(&[("out", app_a_out.as_str())], &[], "x86_64-linux"),
        )
        .unwrap();
    writer
        .add_drv(
            &app_b_drv,
            &synth_aterm(
                &[("out", app_b_out.as_str())],
                &[lib_a_drv.as_str()],
                "x86_64-linux",
            ),
        )
        .unwrap();
    writer
        .add_drv(
            &lib_a_drv,
            &synth_aterm(&[("out", lib_a_out.as_str())], &[], "x86_64-linux"),
        )
        .unwrap();

    // units.jsonl — the two workload units.
    writer
        .write_units(&[
            UnitRecord {
                drv: app_a_drv.clone(),
                label: Some("appA.x86_64-linux".to_string()),
                system: Some("x86_64-linux".to_string()),
                outputs: BTreeMap::from([("out".to_string(), app_a_out.clone())]),
                required_features: Vec::new(),
                identity_divergent: false,
            },
            UnitRecord {
                drv: app_b_drv.clone(),
                label: Some("appB.x86_64-linux".to_string()),
                system: Some("x86_64-linux".to_string()),
                outputs: BTreeMap::from([("out".to_string(), app_b_out.clone())]),
                required_features: Vec::new(),
                identity_divergent: false,
            },
        ])
        .unwrap();

    // requests.jsonl — one recorded client request per unit, on distinct
    // sessions, with the timed offsets the dispatcher tests rely on.
    writer
        .write_requests(&[
            RequestRecord {
                session: 1,
                offset_s: 0.0,
                targets: vec![RequestTarget {
                    drv: app_a_drv.clone(),
                    outputs: vec!["*".to_string()],
                }],
            },
            RequestRecord {
                session: 2,
                offset_s: 1.0,
                targets: vec![RequestTarget {
                    drv: app_b_drv.clone(),
                    outputs: vec!["*".to_string()],
                }],
            },
        ])
        .unwrap();

    // outcomes.jsonl — session-less recorded truth: appA built in 1.0 s;
    // appB disconnected at recorded offset 2.0 s when an interruption is
    // requested, otherwise built like appA.
    let app_b_outcome = if with_interruption {
        OutcomeRecord {
            session: None,
            drv: app_b_drv.clone(),
            outcome: ExpectedOutcome::Disconnected,
            detail: None,
            duration_s: None,
            stop_offset_s: Some(2.0),
            outputs: BTreeMap::new(),
        }
    } else {
        OutcomeRecord {
            session: None,
            drv: app_b_drv.clone(),
            outcome: ExpectedOutcome::Built,
            detail: None,
            duration_s: Some(1.0),
            stop_offset_s: None,
            outputs: BTreeMap::new(),
        }
    };
    writer
        .write_outcomes(&[
            OutcomeRecord {
                session: None,
                drv: app_a_drv.clone(),
                outcome: ExpectedOutcome::Built,
                detail: None,
                duration_s: Some(1.0),
                stop_offset_s: None,
                outputs: BTreeMap::new(),
            },
            app_b_outcome,
        ])
        .unwrap();

    // impure-env.json — when requested, demote appB out of the workload:
    // the recorder observed an impure environment variable for it, so a
    // campaign supplies its outputs instead of rebuilding it.
    if impure_demote_app_b {
        writer
            .write_impure_env(&ImpureEnv::from([(
                app_b_drv.clone(),
                vec!["NIX_SECRET_TOKEN".to_string()],
            )]))
            .unwrap();
    }

    // closures.jsonl — direct adjacency for every derivation.
    writer
        .write_closures(&[
            ClosureRecord {
                drv: app_a_drv.clone(),
                inputs: Vec::new(),
                srcs: Vec::new(),
                outputs: BTreeMap::from([("out".to_string(), Some(app_a_out.clone()))]),
            },
            ClosureRecord {
                drv: app_b_drv.clone(),
                inputs: vec![lib_a_drv.clone()],
                srcs: Vec::new(),
                outputs: BTreeMap::from([("out".to_string(), Some(app_b_out.clone()))]),
            },
            ClosureRecord {
                drv: lib_a_drv.clone(),
                inputs: Vec::new(),
                srcs: Vec::new(),
                outputs: BTreeMap::from([("out".to_string(), Some(lib_a_out.clone()))]),
            },
        ])
        .unwrap();

    // manifest.json — a timed recording with expected outcomes and
    // dependency closures; fixed timestamps keep the fixture deterministic.
    let stamp: jiff::Timestamp = "2026-05-28T00:00:00Z".parse().unwrap();
    let mut provenance = serde_json::Map::new();
    provenance.insert(
        "recorder".to_string(),
        serde_json::Value::from("mini-timed-archive-fixture"),
    );
    let finalized = writer
        .finalize(ManifestSeed {
            created_at: stamp,
            from: stamp,
            to: stamp,
            capabilities: Capabilities {
                timed: true,
                expected_outcomes: true,
                output_hashes: false,
                embedded_store_paths: false,
                impure_env: impure_demote_app_b,
                dependency_closures: true,
            },
            substituters: Substituters {
                relay: vec!["https://cache.example.org".to_string()],
                target: Vec::new(),
            },
            fat: false,
            provenance,
        })
        .unwrap();
    let archive_id_short = crate::archive::identity::short_id(&finalized.archive_id);
    MiniArchive {
        archive_id: finalized.archive_id,
        archive_id_short,
    }
}

/// Write a tiny synthetic directory-form v1 replay archive into `dir`
/// whose recording declares the impure-env capability: two workload units
/// (`pureApp.x86_64-linux`, `impureApp.x86_64-linux`), both with recorded
/// `built` outcomes and requests, and `impure-env.json` listing impure
/// environment variables for impureApp's derivation — the shape the
/// impure-demotion policy (live planner and offline dry-run alike) keys
/// on.
#[cfg(test)]
pub(crate) fn write_mini_impure_archive(dir: &std::path::Path) -> MiniArchive {
    use crate::archive::schema::{
        Capabilities, ExpectedOutcome, OutcomeRecord, RequestRecord, RequestTarget, Substituters,
    };
    use crate::archive::writer::{ArchiveWriter, ManifestSeed};

    let drv = |name: &str| {
        format!(
            "/nix/store/{}-{name}.drv",
            fake_hash(&format!("{name}-drv"))
        )
    };
    let out = |name: &str| format!("/nix/store/{}-{name}", fake_hash(&format!("{name}-out")));

    let pure_drv = drv("pureApp-1.0");
    let pure_out = out("pureApp-1.0");
    let impure_drv = drv("impureApp-1.0");
    let impure_out = out("impureApp-1.0");

    let writer = ArchiveWriter::create(dir).unwrap();
    writer
        .add_drv(
            &pure_drv,
            &synth_aterm(&[("out", pure_out.as_str())], &[], "x86_64-linux"),
        )
        .unwrap();
    writer
        .add_drv(
            &impure_drv,
            &synth_aterm(&[("out", impure_out.as_str())], &[], "x86_64-linux"),
        )
        .unwrap();
    writer
        .write_units(&[
            UnitRecord {
                drv: pure_drv.clone(),
                label: Some("pureApp.x86_64-linux".to_string()),
                system: Some("x86_64-linux".to_string()),
                outputs: BTreeMap::from([("out".to_string(), pure_out.clone())]),
                required_features: Vec::new(),
                identity_divergent: false,
            },
            UnitRecord {
                drv: impure_drv.clone(),
                label: Some("impureApp.x86_64-linux".to_string()),
                system: Some("x86_64-linux".to_string()),
                outputs: BTreeMap::from([("out".to_string(), impure_out.clone())]),
                required_features: Vec::new(),
                identity_divergent: false,
            },
        ])
        .unwrap();
    writer
        .write_requests(
            &[&pure_drv, &impure_drv]
                .into_iter()
                .map(|unit_drv| RequestRecord {
                    session: 0,
                    offset_s: 0.0,
                    targets: vec![RequestTarget {
                        drv: unit_drv.clone(),
                        outputs: vec!["*".to_string()],
                    }],
                })
                .collect::<Vec<_>>(),
        )
        .unwrap();
    writer
        .write_outcomes(
            &[&pure_drv, &impure_drv]
                .into_iter()
                .map(|unit_drv| OutcomeRecord {
                    session: None,
                    drv: unit_drv.clone(),
                    outcome: ExpectedOutcome::Built,
                    detail: None,
                    duration_s: Some(1.0),
                    stop_offset_s: None,
                    outputs: BTreeMap::new(),
                })
                .collect::<Vec<_>>(),
        )
        .unwrap();
    writer
        .write_closures(&[
            ClosureRecord {
                drv: pure_drv.clone(),
                inputs: Vec::new(),
                srcs: Vec::new(),
                outputs: BTreeMap::from([("out".to_string(), Some(pure_out.clone()))]),
            },
            ClosureRecord {
                drv: impure_drv.clone(),
                inputs: Vec::new(),
                srcs: Vec::new(),
                outputs: BTreeMap::from([("out".to_string(), Some(impure_out.clone()))]),
            },
        ])
        .unwrap();
    writer
        .write_impure_env(&BTreeMap::from([(
            impure_drv.clone(),
            vec!["NIX_SECRET_TOKEN".to_string()],
        )]))
        .unwrap();

    let stamp: jiff::Timestamp = "2026-05-28T00:00:00Z".parse().unwrap();
    let mut provenance = serde_json::Map::new();
    provenance.insert(
        "recorder".to_string(),
        serde_json::Value::from("mini-impure-archive-fixture"),
    );
    let finalized = writer
        .finalize(ManifestSeed {
            created_at: stamp,
            from: stamp,
            to: stamp,
            capabilities: Capabilities {
                timed: false,
                expected_outcomes: true,
                output_hashes: false,
                embedded_store_paths: false,
                impure_env: true,
                dependency_closures: true,
            },
            substituters: Substituters {
                relay: vec!["https://cache.example.org".to_string()],
                target: Vec::new(),
            },
            fat: false,
            provenance,
        })
        .unwrap();
    let archive_id_short = crate::archive::identity::short_id(&finalized.archive_id);
    MiniArchive {
        archive_id: finalized.archive_id,
        archive_id_short,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::archive::reader::ReplayArchive;

    #[test]
    fn mini_archive_loads_units_closures_and_exclusions() {
        let tmp = tempfile::tempdir().unwrap();
        let built = write_mini_archive(tmp.path());
        let archive = ReplayArchive::open(tmp.path()).unwrap();
        assert_eq!(built.archive_id.len(), 64);
        assert_eq!(built.archive_id_short, built.archive_id[..16]);

        let units = load_units(&archive).unwrap();
        // same shape the plan stage consumed from manifest.jsonl
        assert_eq!(units.len(), 5);
        let app_b = units.iter().find(|u| u.job == "appB.x86_64-linux").unwrap();
        assert_eq!(app_b.system, "x86_64-linux");
        assert!(app_b.outputs.contains_key("out"));
        // filter-relevant variety carried over from the old mini eval set
        let kvm = units
            .iter()
            .find(|u| u.job == "kvmTest.x86_64-linux")
            .unwrap();
        assert_eq!(kvm.required_features, vec!["kvm".to_string()]);
        assert!(units.iter().any(|u| u.system == "aarch64-linux"));

        let closures = load_closures(&archive, &units).unwrap();
        // per-unit transitive closure reconstructed from the adjacency member,
        // each dep with its declared output paths (CA/None outputs skipped)
        let app_b_clo = closures
            .iter()
            .find(|c| c.job == "appB.x86_64-linux")
            .unwrap();
        let dep_drvs: Vec<&str> = app_b_clo.deps.iter().map(|d| d.drv_path.as_str()).collect();
        assert!(dep_drvs.iter().any(|d| d.contains("-libA-")));
        assert!(
            dep_drvs.iter().any(|d| d.contains("-stdenv-")),
            "transitive dep reached through libA"
        );
        assert!(app_b_clo.deps.iter().all(|d| !d.output_paths.is_empty()));

        let excl = exclusion_counts(&archive);
        assert_eq!(excl.get("eval-error"), Some(&1));
        // The mini archive stages an exclusions member with one record, so
        // the recorder-side exclusion count is present (not the absent-member
        // `None`).
        assert_eq!(exclusions_recorded(&archive), Some(1));
    }

    #[test]
    fn exclusions_recorded_is_none_for_an_exclusions_free_archive() {
        // The timed mini archive stages no exclusions member at all: the
        // recorder made no claim about exclusions, so the count is None (no
        // completeness penalty applies) — distinct from Some(0), which would
        // be a positive "nothing was excluded" claim.
        let tmp = tempfile::tempdir().unwrap();
        write_mini_timed_archive(tmp.path(), false, false);
        let archive = ReplayArchive::open(tmp.path()).unwrap();
        assert_eq!(exclusions_recorded(&archive), None);
        assert!(exclusion_counts(&archive).is_empty());
    }

    #[test]
    fn identity_divergent_units_are_reported() {
        let tmp = tempfile::tempdir().unwrap();
        write_mini_archive(tmp.path());
        let archive = ReplayArchive::open(tmp.path()).unwrap();
        let divergent = identity_divergent_units(&archive).unwrap();
        assert_eq!(divergent, vec!["divergentC.x86_64-linux".to_string()]);
    }

    /// Two-unit archive (appA on x86, libB on aarch64 with a multi-output,
    /// CA-output, feature-requiring derivation) whose `units.jsonl`
    /// coverage is the given subset of the workload. The workload itself
    /// always comes from `requests.jsonl`.
    fn write_units_subset_archive(dir: &std::path::Path, unit_records_for: &[&str]) {
        use crate::archive::schema::{
            Capabilities, ExpectedOutcome, OutcomeRecord, RequestRecord, RequestTarget,
            Substituters,
        };
        use crate::archive::writer::{ArchiveWriter, ManifestSeed};

        let app_a_drv = format!("/nix/store/{}-appA-1.0.drv", fake_hash("appA-drv"));
        let app_a_out = format!("/nix/store/{}-appA-1.0", fake_hash("appA-out"));
        let lib_b_drv = format!("/nix/store/{}-libB-2.0.drv", fake_hash("libB-drv"));
        let lib_b_out = format!("/nix/store/{}-libB-2.0", fake_hash("libB-out"));
        let lib_b_dev = format!("/nix/store/{}-libB-2.0-dev", fake_hash("libB-dev"));

        let writer = ArchiveWriter::create(dir).unwrap();
        writer
            .add_drv(
                &app_a_drv,
                &synth_aterm(&[("out", app_a_out.as_str())], &[], "x86_64-linux"),
            )
            .unwrap();
        // libB: two declared outputs, one floating CA output (empty path),
        // and a requiredSystemFeatures declaration — the metadata shapes
        // ATerm recovery must reproduce.
        writer
            .add_drv(
                &lib_b_drv,
                &synth_aterm_with_env(
                    &[
                        ("dev", lib_b_dev.as_str()),
                        ("doc", ""),
                        ("out", lib_b_out.as_str()),
                    ],
                    &[],
                    "aarch64-linux",
                    &[("requiredSystemFeatures", "kvm big-parallel")],
                ),
            )
            .unwrap();

        if !unit_records_for.is_empty() {
            let records: Vec<UnitRecord> = unit_records_for
                .iter()
                .map(|name| match *name {
                    "appA" => UnitRecord {
                        drv: app_a_drv.clone(),
                        label: Some("appA.x86_64-linux".to_string()),
                        system: Some("x86_64-linux".to_string()),
                        outputs: BTreeMap::from([("out".to_string(), app_a_out.clone())]),
                        required_features: Vec::new(),
                        identity_divergent: false,
                    },
                    other => panic!("unknown unit fixture {other}"),
                })
                .collect();
            writer.write_units(&records).unwrap();
        }

        writer
            .write_requests(&[
                RequestRecord {
                    session: 0,
                    offset_s: 0.0,
                    targets: vec![RequestTarget {
                        drv: app_a_drv.clone(),
                        outputs: vec!["*".to_string()],
                    }],
                },
                RequestRecord {
                    session: 0,
                    offset_s: 0.0,
                    targets: vec![RequestTarget {
                        drv: lib_b_drv.clone(),
                        outputs: vec!["*".to_string()],
                    }],
                },
            ])
            .unwrap();
        writer
            .write_outcomes(&[OutcomeRecord {
                session: None,
                drv: app_a_drv.clone(),
                outcome: ExpectedOutcome::Built,
                detail: None,
                duration_s: None,
                stop_offset_s: None,
                outputs: BTreeMap::new(),
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
                    expected_outcomes: true,
                    output_hashes: false,
                    embedded_store_paths: false,
                    impure_env: false,
                    dependency_closures: false,
                },
                substituters: Substituters {
                    relay: Vec::new(),
                    target: Vec::new(),
                },
                fat: false,
                provenance: serde_json::Map::new(),
            })
            .unwrap();
    }

    #[test]
    fn load_units_recovers_the_workload_from_aterms_without_units_jsonl() {
        // A spec-conforming archive without units.jsonl: the workload comes
        // from requests.jsonl and every entry's metadata is recovered from
        // the embedded derivation — never an empty (zero-unit) plan and
        // never an outputs-less placeholder.
        let tmp = tempfile::tempdir().unwrap();
        write_units_subset_archive(tmp.path(), &[]);
        let archive = ReplayArchive::open(tmp.path()).unwrap();
        assert!(archive.units().is_empty(), "fixture stages no units.jsonl");
        assert_eq!(archive.workload_units().len(), 2);

        let units = load_units(&archive).unwrap();
        assert_eq!(units.len(), 2, "the full request workload is planned");

        let lib_b = units.iter().find(|u| u.job.contains("libB")).unwrap();
        // job/attr fall back to the drv basename without the .drv suffix
        assert!(lib_b.job.ends_with("-libB-2.0"), "got: {}", lib_b.job);
        assert_eq!(lib_b.attr, lib_b.job);
        assert_eq!(lib_b.system, "aarch64-linux");
        // declared outputs recovered; the floating CA output (empty path)
        // is excluded exactly as recorders exclude it from units.jsonl
        assert_eq!(lib_b.outputs.len(), 2);
        assert!(lib_b.outputs.contains_key("out"));
        assert!(lib_b.outputs.contains_key("dev"));
        assert!(!lib_b.outputs.contains_key("doc"));
        assert_eq!(
            lib_b.required_features,
            vec!["kvm".to_string(), "big-parallel".to_string()],
            "requiredSystemFeatures recovered in declaration order"
        );

        let app_a = units.iter().find(|u| u.job.contains("appA")).unwrap();
        assert_eq!(app_a.system, "x86_64-linux");
        assert_eq!(app_a.outputs.len(), 1);
    }

    #[test]
    fn load_units_keeps_workload_targets_not_covered_by_units_jsonl() {
        // units.jsonl covering a subset of the request targets must not
        // shrink the workload: covered units keep their recorded label,
        // uncovered ones are recovered from their ATerm.
        let tmp = tempfile::tempdir().unwrap();
        write_units_subset_archive(tmp.path(), &["appA"]);
        let archive = ReplayArchive::open(tmp.path()).unwrap();
        assert_eq!(archive.units().len(), 1);

        let units = load_units(&archive).unwrap();
        assert_eq!(units.len(), 2, "the uncovered target is not dropped");
        assert!(units.iter().any(|u| u.job == "appA.x86_64-linux"));
        let lib_b = units.iter().find(|u| u.job.contains("libB")).unwrap();
        assert_eq!(lib_b.system, "aarch64-linux");
        assert!(!lib_b.outputs.is_empty());
    }

    #[test]
    fn load_units_fails_loudly_when_a_recoverable_unit_has_no_drv_member() {
        // A workload unit with neither a units.jsonl record nor a readable
        // embedded derivation cannot say what it produces; that is a
        // per-unit hard error naming the derivation, never a silent skip.
        let tmp = tempfile::tempdir().unwrap();
        write_units_subset_archive(tmp.path(), &[]);
        let lib_b_drv_name = format!("{}-libB-2.0.drv", fake_hash("libB-drv"));
        std::fs::remove_file(tmp.path().join("nix/store").join(&lib_b_drv_name)).unwrap();

        let archive = ReplayArchive::open(tmp.path()).unwrap();
        let err = format!("{:#}", load_units(&archive).unwrap_err());
        assert!(err.contains("no units.jsonl record"), "got: {err}");
        assert!(err.contains("libB-2.0.drv"), "error names the drv: {err}");
    }

    /// One-unit archive with the dependency chain app → libC → stdenv in
    /// the embedded ATerms only: no `units.jsonl`, no `closures.jsonl`,
    /// `dependency_closures` false — the minimal capability surface a v0
    /// archive (or a recorder that skips the ATerm pass) presents.
    fn write_capabilityless_chain_archive(dir: &std::path::Path) {
        use crate::archive::schema::{
            Capabilities, ExpectedOutcome, OutcomeRecord, RequestRecord, RequestTarget,
            Substituters,
        };
        use crate::archive::writer::{ArchiveWriter, ManifestSeed};

        let app_drv = format!("/nix/store/{}-app-3.0.drv", fake_hash("chain-app-drv"));
        let app_out = format!("/nix/store/{}-app-3.0", fake_hash("chain-app-out"));
        let lib_c_drv = format!("/nix/store/{}-libC-1.0.drv", fake_hash("chain-libC-drv"));
        let lib_c_out = format!("/nix/store/{}-libC-1.0", fake_hash("chain-libC-out"));
        let stdenv_drv = format!("/nix/store/{}-stdenv-9.drv", fake_hash("chain-stdenv-drv"));
        let stdenv_out = format!("/nix/store/{}-stdenv-9", fake_hash("chain-stdenv-out"));

        let writer = ArchiveWriter::create(dir).unwrap();
        writer
            .add_drv(
                &app_drv,
                &synth_aterm(
                    &[("out", app_out.as_str())],
                    &[lib_c_drv.as_str()],
                    "x86_64-linux",
                ),
            )
            .unwrap();
        writer
            .add_drv(
                &lib_c_drv,
                &synth_aterm(
                    &[("out", lib_c_out.as_str())],
                    &[stdenv_drv.as_str()],
                    "x86_64-linux",
                ),
            )
            .unwrap();
        writer
            .add_drv(
                &stdenv_drv,
                &synth_aterm(&[("out", stdenv_out.as_str())], &[], "x86_64-linux"),
            )
            .unwrap();
        writer
            .write_requests(&[RequestRecord {
                session: 0,
                offset_s: 0.0,
                targets: vec![RequestTarget {
                    drv: app_drv.clone(),
                    outputs: vec!["*".to_string()],
                }],
            }])
            .unwrap();
        writer
            .write_outcomes(&[OutcomeRecord {
                session: None,
                drv: app_drv.clone(),
                outcome: ExpectedOutcome::Built,
                detail: None,
                duration_s: None,
                stop_offset_s: None,
                outputs: BTreeMap::new(),
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
                    expected_outcomes: true,
                    output_hashes: false,
                    embedded_store_paths: false,
                    impure_env: false,
                    dependency_closures: false,
                },
                substituters: Substituters::default(),
                fat: false,
                provenance: serde_json::Map::new(),
            })
            .unwrap();
    }

    #[test]
    fn load_closures_falls_back_to_the_embedded_aterm_walk() {
        // Without the dependency_closures capability the adjacency is
        // recovered from the embedded derivations — the same per-unit
        // transitive closure shape closures.jsonl would have produced,
        // instead of refusing the archive.
        let tmp = tempfile::tempdir().unwrap();
        write_capabilityless_chain_archive(tmp.path());
        let archive = ReplayArchive::open(tmp.path()).unwrap();
        assert!(!archive.capabilities().dependency_closures);
        assert!(archive.closures().is_empty());

        let units = load_units(&archive).unwrap();
        assert_eq!(units.len(), 1, "the workload itself comes from requests");

        let closures = load_closures(&archive, &units).unwrap();
        assert_eq!(closures.len(), 1);
        let app = &closures[0];
        let dep_drvs: Vec<&str> = app.deps.iter().map(|d| d.drv_path.as_str()).collect();
        assert_eq!(dep_drvs.len(), 2);
        assert!(dep_drvs.iter().any(|d| d.contains("-libC-")));
        assert!(
            dep_drvs.iter().any(|d| d.contains("-stdenv-")),
            "transitive dep reached through libC: {dep_drvs:?}"
        );
        assert!(
            app.deps.iter().all(|d| !d.output_paths.is_empty()),
            "declared output paths recovered from the ATerms"
        );
    }

    #[test]
    fn aterm_fallback_fails_loudly_when_a_dependency_drv_is_missing() {
        // The fallback walk reads real derivations; a dependency the
        // archive does not embed is a hard error naming the derivation and
        // the unit whose closure needed it — never a silently shallower
        // closure.
        let tmp = tempfile::tempdir().unwrap();
        write_capabilityless_chain_archive(tmp.path());
        let lib_c_drv_name = format!("{}-libC-1.0.drv", fake_hash("chain-libC-drv"));
        std::fs::remove_file(tmp.path().join("nix/store").join(&lib_c_drv_name)).unwrap();

        let archive = ReplayArchive::open(tmp.path()).unwrap();
        let units = load_units(&archive).unwrap();
        let err = format!("{:#}", load_closures(&archive, &units).unwrap_err());
        assert!(err.contains("libC-1.0.drv"), "got: {err}");
        assert!(err.contains("-app-3.0.drv"), "error names the root: {err}");
    }
}
