//! Adapters from an open replay archive to the engine's plan inputs
//! (units, per-unit dependency closures, exclusions), plus the synthetic
//! mini-archive used by the engine's offline tests.
//!
//! The plan stage keeps consuming the same in-memory shapes it used for
//! the legacy eval-set artifacts ([`ManifestEntry`], [`DepClosureEntry`]);
//! these adapters fill those shapes from an open
//! [`ReplayArchive`] instead:
//! `units.jsonl` records become manifest entries, the per-derivation
//! adjacency in `closures.jsonl` is expanded into per-unit transitive
//! dependency closures, and `exclusions.jsonl` feeds the plan-time
//! completeness accounting.

use std::collections::{BTreeMap, BTreeSet, HashMap, VecDeque};

use anyhow::Result;
use serde::{Deserialize, Serialize};

use crate::archive::reader::ReplayArchive;
use crate::archive::schema::{ClosureRecord, UnitRecord};

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

/// Workload units from the archive's `units.jsonl`, one [`ManifestEntry`]
/// per record: `job`/`attr` from the record's label (drv basename without
/// the `.drv` suffix when the label is absent), `system` (empty string
/// when absent), declared outputs, and required features.
///
/// Records for derivations no recorded request targets are skipped with a
/// warning (the reader already drops them at open; the check here is a
/// second line of defense). The returned entries are sorted by job then
/// drv path so plan-time iteration order is deterministic — the archive's
/// unit map carries no inherent order.
pub fn load_units(archive: &ReplayArchive) -> Result<Vec<ManifestEntry>> {
    let workload = archive.workload_units();
    let mut units = Vec::with_capacity(archive.units().len());
    for record in archive.units().values() {
        if !workload.contains(&record.drv) {
            tracing::warn!(
                drv = %record.drv,
                "skipping a units.jsonl record for a derivation no recorded request targets"
            );
            continue;
        }
        let job = unit_job(record);
        units.push(ManifestEntry {
            job: job.clone(),
            system: record.system.clone().unwrap_or_default(),
            attr: job,
            drv_path: record.drv.clone(),
            outputs: record.outputs.clone(),
            required_features: record.required_features.clone(),
        });
    }
    units.sort_by(|a, b| a.job.cmp(&b.job).then_with(|| a.drv_path.cmp(&b.drv_path)));
    Ok(units)
}

/// Per-unit proper transitive dependency closures, reconstructed from the
/// archive's direct-adjacency `closures.jsonl`: for each unit, walk the
/// `inputs` edges breadth-first and emit every reachable derivation
/// (the unit itself excluded) with its statically declared output paths
/// (floating content-addressed outputs, recorded as null, are skipped).
///
/// Requires the `dependency_closures` capability: without `closures.jsonl`
/// the engine cannot compute warm sets or leaf-mode attemptability.
pub fn load_closures(
    archive: &ReplayArchive,
    units: &[ManifestEntry],
) -> Result<Vec<DepClosureEntry>> {
    anyhow::ensure!(
        archive.capabilities().dependency_closures,
        "archive lacks dependency_closures; the engine's ATerm fallback walk is not implemented \
         — re-record the archive with closures.jsonl"
    );
    let adjacency: HashMap<&str, &ClosureRecord> = archive
        .closures()
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

/// Count of `exclusions.jsonl` records per exclusion reason (empty when
/// the archive carries no exclusions member).
pub fn exclusion_counts(archive: &ReplayArchive) -> BTreeMap<String, usize> {
    let mut counts: BTreeMap<String, usize> = BTreeMap::new();
    for record in archive.exclusions() {
        *counts.entry(record.reason.clone()).or_insert(0) += 1;
    }
    counts
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
        None => {
            let base = record.drv.rsplit('/').next().unwrap_or(&record.drv);
            base.strip_suffix(".drv").unwrap_or(base).to_string()
        }
    }
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
                        nar_hash_hex: "ab".repeat(32),
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
    }

    #[test]
    fn identity_divergent_units_are_reported() {
        let tmp = tempfile::tempdir().unwrap();
        write_mini_archive(tmp.path());
        let archive = ReplayArchive::open(tmp.path()).unwrap();
        let divergent = identity_divergent_units(&archive).unwrap();
        assert_eq!(divergent, vec!["divergentC.x86_64-linux".to_string()]);
    }
}
