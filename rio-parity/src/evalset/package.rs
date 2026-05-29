//! Stages the eval pipeline's outputs as a v1 replay archive.
//!
//! [`stage_archive`] turns the recorder's in-memory results — manifest
//! records, eval errors, excluded aggregates, the union closure
//! adjacency, the swept expected outcomes, the fidelity report, and the
//! provenance block — into the directory form of a v1 replay archive
//! via [`crate::archive::writer::ArchiveWriter`]: it derives
//! `units.jsonl`, synthesizes one timeless request per unit, copies the
//! closure `.drv` texts out of the local store, and lets the writer's
//! `finalize` compute the integrity tables and the archive id. Packing
//! the staged directory into the published DwarFS image is the separate
//! [`crate::archive::writer::pack_with_mkdwarfs`] step;
//! [`mkdwarfs_version`] only probes the tool version for provenance.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use anyhow::Context as _;

use crate::archive::identity;
use crate::archive::schema::{
    Capabilities, EXCLUSION_REASON_AGGREGATE, EXCLUSION_REASON_EVAL_ERROR, ExclusionRecord,
    ImpureEnv, OutcomeRecord, RequestRecord, RequestTarget, Substituters, UnitRecord,
};
use crate::archive::writer::{ArchiveWriter, ManifestSeed};
use crate::evalset::artifacts::FIDELITY_FILE;
use crate::evalset::depclosure::ClosureAdjacency;
use crate::evalset::evaluator::{EvalErrorRecord, ManifestRecord};
use crate::evalset::fidelity::FidelityReport;

/// Everything the recorder has gathered by the time it stages an archive.
///
/// The borrowed fields are the eval pipeline's own working state (kept
/// alive by the caller for later phases); the owned fields are produced
/// specifically for the archive and handed over here.
#[derive(Debug)]
pub struct StageInputs<'a> {
    /// One record per workload unit, as produced by the evaluator (with
    /// `requiredFeatures` already backfilled from the closure pass).
    pub manifest: &'a [ManifestRecord],
    /// Attributes that failed to evaluate; staged as `eval-error`
    /// exclusions.
    pub eval_errors: &'a [EvalErrorRecord],
    /// `(attr, drvPath)` of aggregate jobs excluded from the build scope;
    /// staged as `aggregate` exclusions.
    pub aggregates: &'a [(String, String)],
    /// Union closure adjacency over every workload unit (records become
    /// `closures.jsonl`; the impure-env map, restricted to workload
    /// units, becomes `impure-env.json`).
    pub adjacency: &'a ClosureAdjacency,
    /// Expected-outcome records from the truth sweep, written verbatim as
    /// `outcomes.jsonl`.
    pub outcomes: Vec<OutcomeRecord>,
    /// The drvPath fidelity report. Units named in its mismatches are
    /// marked `identity_divergent`; the report itself is kept verbatim as
    /// the extra `fidelity.json` member (ignored by readers, outside the
    /// archive identity).
    pub fidelity: &'a FidelityReport,
    /// The recorder's provenance block, carried verbatim into the
    /// manifest. Must be a JSON object.
    pub provenance: serde_json::Value,
    /// Binary caches the engine may relay content from at campaign time
    /// (becomes `substituters.relay`; the target list is left empty).
    pub relay_substituters: Vec<String>,
    /// When the upstream truth sweep started. The archive is timeless, so
    /// this single instant becomes both `from` and `to`.
    pub truth_swept_at: jiff::Timestamp,
    /// ATerm text to use instead of reading a derivation from the local
    /// store, keyed by drv store path. Offline tests stage synthetic
    /// derivations through this map; the production recorder leaves it
    /// empty and reads every closure member from the store it just
    /// evaluated into.
    pub drv_text_overrides: BTreeMap<String, String>,
}

/// A finalized staging directory and its content-derived identity.
#[derive(Debug, Clone)]
pub struct StagedArchive {
    /// The staged archive root (directory form, ready to pack).
    pub dir: PathBuf,
    /// Full 64-hex archive id (SHA-256 of the staged `manifest.json`).
    pub archive_id: String,
    /// First 16 hex characters of the archive id (S3 prefix segment).
    pub archive_id_short: String,
}

/// Stage a v1 replay archive directory at `dir` from the recorder's
/// gathered outputs.
///
/// Member derivation:
/// - `units.jsonl`: one record per manifest record (`label` = job),
///   `identity_divergent` set for jobs named in the fidelity report's
///   mismatches;
/// - `requests.jsonl`: one synthesized timeless request per unit
///   (`session` 0, all outputs);
/// - `closures.jsonl` / `impure-env.json`: straight from the closure
///   adjacency (the impure-env map restricted to workload units, and
///   only written when that restriction is non-empty);
/// - `exclusions.jsonl`: eval errors and excluded aggregates;
/// - `outcomes.jsonl`: the swept expected outcomes, verbatim;
/// - `nix/store/*.drv`: the ATerm text of every closure derivation, read
///   from the local store unless overridden;
/// - `fidelity.json`: the fidelity report, kept as an extra member.
///
/// The archive claims `expected_outcomes`, `output_hashes`, and
/// `dependency_closures`; `impure_env` only when the member was written;
/// never `timed`, `embedded_store_paths`, or `fat`. `from == to ==
/// truth_swept_at` (a timeless archive records an instant, not a window).
pub async fn stage_archive(dir: &Path, inputs: &StageInputs<'_>) -> anyhow::Result<StagedArchive> {
    let provenance = match &inputs.provenance {
        serde_json::Value::Object(map) => map.clone(),
        other => anyhow::bail!(
            "archive provenance must be a JSON object, got {}",
            json_kind(other)
        ),
    };

    let writer = ArchiveWriter::create(dir)
        .with_context(|| format!("create archive staging directory {}", dir.display()))?;

    // units.jsonl — one record per workload unit. A unit is identity
    // divergent when the fidelity gate found its locally evaluated drvPath
    // differing from the source's.
    let divergent_jobs: BTreeSet<&str> = inputs
        .fidelity
        .mismatches
        .iter()
        .map(|mismatch| mismatch.job.as_str())
        .collect();
    let units: Vec<UnitRecord> = inputs
        .manifest
        .iter()
        .map(|record| UnitRecord {
            drv: record.drv_path.clone(),
            label: Some(record.job.clone()),
            system: Some(record.system.clone()),
            outputs: record.outputs.clone(),
            required_features: record.required_features.clone().unwrap_or_default(),
            identity_divergent: divergent_jobs.contains(record.job.as_str()),
        })
        .collect();
    writer.write_units(&units)?;

    // requests.jsonl — the recorder is timeless: one synthesized request
    // per unit, all in session 0 with no meaningful offset, asking for
    // every output.
    let requests: Vec<RequestRecord> = inputs
        .manifest
        .iter()
        .map(|record| RequestRecord {
            session: 0,
            offset_s: 0.0,
            targets: vec![RequestTarget {
                drv: record.drv_path.clone(),
                outputs: vec!["*".to_string()],
            }],
        })
        .collect();
    writer.write_requests(&requests)?;

    // closures.jsonl — the adjacency records are already in the member's
    // shape, one per derivation in the union closure.
    let closures: Vec<_> = inputs.adjacency.records.values().cloned().collect();
    writer.write_closures(&closures)?;

    // exclusions.jsonl — scope items that produced no workload unit:
    // attributes that failed to evaluate and aggregates excluded from the
    // build scope.
    let mut exclusions: Vec<ExclusionRecord> =
        Vec::with_capacity(inputs.eval_errors.len() + inputs.aggregates.len());
    for error in inputs.eval_errors {
        exclusions.push(ExclusionRecord {
            label: Some(error.attr.clone()),
            drv: None,
            reason: EXCLUSION_REASON_EVAL_ERROR.to_string(),
            detail: Some(error.error.clone()),
        });
    }
    for (attr, drv_path) in inputs.aggregates {
        exclusions.push(ExclusionRecord {
            label: Some(attr.clone()),
            drv: Some(drv_path.clone()),
            reason: EXCLUSION_REASON_AGGREGATE.to_string(),
            detail: None,
        });
    }
    writer.write_exclusions(&exclusions)?;

    // outcomes.jsonl — the truth sweep's records, verbatim.
    writer.write_outcomes(&inputs.outcomes)?;

    // impure-env.json — only the workload units' declarations matter for
    // demotion, and the member is omitted entirely when no unit declares
    // any (its capability flag mirrors the member's presence).
    let workload: BTreeSet<&str> = inputs
        .manifest
        .iter()
        .map(|record| record.drv_path.as_str())
        .collect();
    let impure_env: ImpureEnv = inputs
        .adjacency
        .impure_env
        .iter()
        .filter(|(drv, _)| workload.contains(drv.as_str()))
        .map(|(drv, names)| (drv.clone(), names.clone()))
        .collect();
    let wrote_impure_env = !impure_env.is_empty();
    if wrote_impure_env {
        writer.write_impure_env(&impure_env)?;
    }

    // nix/store/<basename>.drv — the ATerm text of every derivation in the
    // union closure. The recorder runs on the host that just evaluated, so
    // every closure derivation exists in its local store; a missing file is
    // a hard error naming the path.
    for drv_path in inputs.adjacency.records.keys() {
        let aterm = match inputs.drv_text_overrides.get(drv_path) {
            Some(text) => text.clone(),
            None => std::fs::read_to_string(drv_path).with_context(|| {
                format!("read closure derivation {drv_path} from the local store")
            })?,
        };
        writer.add_drv(drv_path, &aterm)?;
    }

    // fidelity.json — the recorder's own QA artifact, staged next to the
    // known members. Readers ignore it and the manifest never lists it, so
    // it stays outside the archive identity.
    let fidelity_path = dir.join(FIDELITY_FILE);
    let mut fidelity_bytes = serde_json::to_vec_pretty(inputs.fidelity)
        .context("serialize the fidelity report for the staged archive")?;
    fidelity_bytes.push(b'\n');
    std::fs::write(&fidelity_path, fidelity_bytes)
        .with_context(|| format!("write {}", fidelity_path.display()))?;

    // The manifest seed: a timeless archive whose recorded window is the
    // single instant the truth sweep started, claiming exactly the members
    // staged above.
    let finalized = writer.finalize(ManifestSeed {
        created_at: jiff::Timestamp::now(),
        from: inputs.truth_swept_at,
        to: inputs.truth_swept_at,
        capabilities: Capabilities {
            timed: false,
            expected_outcomes: true,
            output_hashes: true,
            embedded_store_paths: false,
            impure_env: wrote_impure_env,
            dependency_closures: true,
        },
        substituters: Substituters {
            relay: inputs.relay_substituters.clone(),
            target: Vec::new(),
        },
        fat: false,
        provenance,
    })?;
    let archive_id_short = identity::short_id(&finalized.archive_id);
    Ok(StagedArchive {
        dir: dir.to_path_buf(),
        archive_id: finalized.archive_id,
        archive_id_short,
    })
}

/// Short JSON type name for error messages about non-object provenance.
fn json_kind(value: &serde_json::Value) -> &'static str {
    match value {
        serde_json::Value::Null => "null",
        serde_json::Value::Bool(_) => "a boolean",
        serde_json::Value::Number(_) => "a number",
        serde_json::Value::String(_) => "a string",
        serde_json::Value::Array(_) => "an array",
        serde_json::Value::Object(_) => "an object",
    }
}

/// Probe the `mkdwarfs` version string for the archive provenance. The
/// packer itself is [`crate::archive::writer::pack_with_mkdwarfs`]; this
/// only records which tool produced the published image.
///
/// `mkdwarfs --version` is tried first, but dwarfs 0.14.x rejects the
/// flag (exit 1, "unrecognised option"), so the probe falls back to the
/// `--help` output, whose banner carries the same `mkdwarfs (vX.Y.Z)`
/// line a few rows below the ASCII-art header. Only when neither output
/// contains that banner line does the probe error.
pub async fn mkdwarfs_version() -> anyhow::Result<String> {
    // Non-UTF-8 probe output is treated as carrying no banner line (empty
    // text) rather than as an error: the fallback below and the final bail
    // already cover "nothing usable came out".
    let version_out = run_mkdwarfs(&["--version"]).await?;
    let version_stdout = std::str::from_utf8(&version_out.stdout).unwrap_or_default();
    if let Some(version) = version_out
        .status
        .success()
        .then(|| parse_mkdwarfs_version_banner(version_stdout))
        .flatten()
    {
        return Ok(version);
    }

    let help_out = run_mkdwarfs(&["--help"]).await?;
    let help_stdout = std::str::from_utf8(&help_out.stdout).unwrap_or_default();
    let help_stderr = std::str::from_utf8(&help_out.stderr).unwrap_or_default();
    if let Some(version) = parse_mkdwarfs_version_banner(help_stdout)
        .or_else(|| parse_mkdwarfs_version_banner(help_stderr))
    {
        return Ok(version);
    }

    let help_text = if help_stdout.trim().is_empty() {
        help_stderr
    } else {
        help_stdout
    };
    anyhow::bail!(
        "could not determine the mkdwarfs version: `mkdwarfs --version` exited {} ({}) and the \
         `mkdwarfs --help` output (exit {}) carries no `mkdwarfs (...)` banner line: {}",
        version_out.status,
        crate::body_snippet(
            std::str::from_utf8(&version_out.stderr).unwrap_or("<non-utf8 stderr>")
        ),
        help_out.status,
        crate::body_snippet(help_text),
    );
}

/// Run `mkdwarfs` with `args` and capture its output. Used only by the
/// version probe; packing goes through the archive writer's packer.
async fn run_mkdwarfs(args: &[&str]) -> anyhow::Result<std::process::Output> {
    tokio::process::Command::new("mkdwarfs")
        .args(args)
        // Cancelling the caller drops this future; kill_on_drop keeps that
        // from orphaning a still-running mkdwarfs process.
        .kill_on_drop(true)
        .output()
        .await
        .with_context(|| {
            format!(
                "spawn mkdwarfs {} (is the dwarfs package in the environment?)",
                args.join(" ")
            )
        })
}

/// Pick the version line out of `mkdwarfs` output: the first line that
/// starts with `mkdwarfs (`, trimmed. Releases that support `--version`
/// print it as the first output line; 0.14.x only prints it inside the
/// `--help` banner, below the ASCII-art header.
fn parse_mkdwarfs_version_banner(output: &str) -> Option<String> {
    output
        .lines()
        .map(str::trim)
        .find(|line| line.starts_with("mkdwarfs ("))
        .map(str::to_string)
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::*;
    use crate::archive::reader::ReplayArchive;
    use crate::archive::schema::{ClosureRecord, ExpectedOutcome, OutputHash};
    use crate::evalset::Scope;
    use crate::evalset::fidelity::{FidelityMismatch, MODE_EXHAUSTIVE};
    use crate::evalset::key::EvalSetKey;

    const APP_A_DRV: &str = "/nix/store/a1111111111111111111111111111111-app-a-1.0.drv";
    const APP_A_OUT: &str = "/nix/store/a4444444444444444444444444444444-app-a-1.0";
    const APP_B_DRV: &str = "/nix/store/b2222222222222222222222222222222-app-b-2.0.drv";
    const APP_B_OUT: &str = "/nix/store/b5555555555555555555555555555555-app-b-2.0";
    const DEP_DRV: &str = "/nix/store/c3333333333333333333333333333333-dep-0.1.drv";
    const DEP_OUT: &str = "/nix/store/c6666666666666666666666666666666-dep-0.1";
    const AGG_DRV: &str = "/nix/store/d7777777777777777777777777777777-tested.drv";

    /// Synthetic ATerm for a leaf derivation (no input derivations).
    fn leaf_aterm(out_path: &str) -> String {
        format!(
            r#"Derive([("out","{out_path}","","")],[],[],"x86_64-linux","/bin/sh",["-c","true"],[("out","{out_path}")])"#
        ) + "\n"
    }

    /// Synthetic ATerm for a derivation depending on `dep_drv`'s `out`.
    fn dependent_aterm(out_path: &str, dep_drv: &str) -> String {
        format!(
            r#"Derive([("out","{out_path}","","")],[("{dep_drv}",["out"])],[],"x86_64-linux","/bin/sh",["-c","true"],[("out","{out_path}")])"#
        ) + "\n"
    }

    /// Owned test data mirroring everything the eval pipeline hands to
    /// [`stage_archive`], plus the convenience fields the assertions use.
    /// [`StageInputs`] borrows the bulk members, so the owner lives here
    /// and [`TestInputs::stage`] lends the borrowed view.
    struct TestInputs {
        manifest: Vec<ManifestRecord>,
        eval_errors: Vec<EvalErrorRecord>,
        aggregates: Vec<(String, String)>,
        adjacency: ClosureAdjacency,
        outcomes: Vec<OutcomeRecord>,
        fidelity: FidelityReport,
        provenance: serde_json::Value,
        relay_substituters: Vec<String>,
        truth_swept_at: jiff::Timestamp,
        drv_text_overrides: BTreeMap<String, String>,
        recipe_digest: String,
        some_unit_drv: String,
    }

    impl TestInputs {
        fn stage(&self) -> StageInputs<'_> {
            StageInputs {
                manifest: &self.manifest,
                eval_errors: &self.eval_errors,
                aggregates: &self.aggregates,
                adjacency: &self.adjacency,
                outcomes: self.outcomes.clone(),
                fidelity: &self.fidelity,
                provenance: self.provenance.clone(),
                relay_substituters: self.relay_substituters.clone(),
                truth_swept_at: self.truth_swept_at,
                drv_text_overrides: self.drv_text_overrides.clone(),
            }
        }
    }

    /// Two workload units (one identity divergent), one eval error, one
    /// excluded aggregate, a three-derivation closure with synthetic ATerm
    /// texts, one built-with-hashes and one failed outcome, impure env on
    /// one unit, and a recorder-shaped provenance block.
    fn test_inputs() -> TestInputs {
        let manifest = vec![
            ManifestRecord {
                job: "appA.x86_64-linux".to_string(),
                system: "x86_64-linux".to_string(),
                attr: "appA.x86_64-linux".to_string(),
                drv_path: APP_A_DRV.to_string(),
                outputs: BTreeMap::from([("out".to_string(), APP_A_OUT.to_string())]),
                required_features: Some(vec!["big-parallel".to_string()]),
            },
            ManifestRecord {
                job: "divergentB.x86_64-linux".to_string(),
                system: "x86_64-linux".to_string(),
                attr: "divergentB.x86_64-linux".to_string(),
                drv_path: APP_B_DRV.to_string(),
                outputs: BTreeMap::from([("out".to_string(), APP_B_OUT.to_string())]),
                required_features: None,
            },
        ];
        let eval_errors = vec![EvalErrorRecord {
            attr: "broken.x86_64-linux".to_string(),
            error: "attribute 'missing' not found".to_string(),
        }];
        let aggregates = vec![("tested.x86_64-linux".to_string(), AGG_DRV.to_string())];

        let mut adjacency = ClosureAdjacency::default();
        for (drv, out, inputs) in [
            (APP_A_DRV, APP_A_OUT, vec![DEP_DRV.to_string()]),
            (APP_B_DRV, APP_B_OUT, vec![DEP_DRV.to_string()]),
            (DEP_DRV, DEP_OUT, Vec::new()),
        ] {
            adjacency.records.insert(
                drv.to_string(),
                ClosureRecord {
                    drv: drv.to_string(),
                    inputs,
                    srcs: Vec::new(),
                    outputs: BTreeMap::from([("out".to_string(), Some(out.to_string()))]),
                },
            );
        }
        adjacency.impure_env.insert(
            APP_A_DRV.to_string(),
            vec!["http_proxy".to_string(), "https_proxy".to_string()],
        );
        // Impure env declared by a non-workload closure member must not
        // reach the staged member (only workload units are demotable).
        adjacency
            .impure_env
            .insert(DEP_DRV.to_string(), vec!["NIX_SECRET".to_string()]);

        let drv_text_overrides = BTreeMap::from([
            (APP_A_DRV.to_string(), dependent_aterm(APP_A_OUT, DEP_DRV)),
            (APP_B_DRV.to_string(), dependent_aterm(APP_B_OUT, DEP_DRV)),
            (DEP_DRV.to_string(), leaf_aterm(DEP_OUT)),
        ]);

        let outcomes = vec![
            OutcomeRecord {
                session: None,
                drv: APP_A_DRV.to_string(),
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
                drv: APP_B_DRV.to_string(),
                outcome: ExpectedOutcome::Failed,
                detail: Some("hydra buildstatus=1".to_string()),
                duration_s: None,
                stop_offset_s: None,
                outputs: BTreeMap::new(),
            },
        ];

        // The fidelity gate found divergentB's drvPath differing from the
        // source, which is what marks that unit identity divergent.
        let fidelity = FidelityReport {
            mode: MODE_EXHAUSTIVE.to_string(),
            checked: 2,
            matched: 1,
            mismatches: vec![FidelityMismatch {
                job: "divergentB.x86_64-linux".to_string(),
                local_drv: APP_B_DRV.to_string(),
                hydra_drv: "/nix/store/d9999999999999999999999999999999-app-b-2.0.drv".to_string(),
            }],
            missing_locally: Vec::new(),
            missing_on_hydra: Vec::new(),
            divergent: true,
        };

        let recipe = EvalSetKey {
            hydra_eval_id: 1_824_219,
            project: "nixpkgs".to_string(),
            jobset: "trunk".to_string(),
            systems: vec!["x86_64-linux".to_string()],
            scope: Scope::Jobs {
                jobs: vec![
                    "appA.x86_64-linux".to_string(),
                    "divergentB.x86_64-linux".to_string(),
                ],
            },
            engine_version: "0.1.0".to_string(),
            nix_version: "nix (Nix) 2.34.0".to_string(),
            nix_eval_jobs_version: "nix-eval-jobs 2.30.0".to_string(),
            args_expr_sha256: "0".repeat(64),
            forced_at: None,
        };
        let recipe_digest = recipe.digest();

        // The provenance block is opaque to the staging builder and must
        // survive verbatim; its values here are recorder-shaped but
        // deliberately independent of the other test inputs (the builder
        // never derives or cross-checks provenance content).
        let provenance = serde_json::json!({
            "recorder": "rio-parity-eval",
            "recorder_version": "0.1.0",
            "recipe_digest": recipe_digest,
            "recipe": serde_json::to_value(&recipe).unwrap(),
            "source": {
                "kind": "hydra",
                "hydra_eval_id": 1_824_219,
                "project": "nixpkgs",
                "jobset": "trunk",
                "nixpkgs_revision": "0123456789abcdef0123456789abcdef01234567",
                "rev_count": 700_000,
                "short_rev": "0123456",
                "source_store_path": "/nix/store/s8888888888888888888888888888888-source",
                "jobset_config": {"type": 1},
            },
            "evaluator": {
                "program": "nix-eval-jobs",
                "argv": ["--workers", "4", "--gc-roots-dir", "/tmp/roots"],
            },
            "fidelity": {
                "mode": "exhaustive",
                "checked": 2,
                "matched": 2,
                "mismatch_count": 0,
                "divergent": false,
            },
            "stats": {
                "in_scope_jobs": 2,
                "manifest_records": 2,
                "eval_errors": 1,
                "aggregates_excluded": 1,
                "closure_drvs": 3,
                "ca_outputs": 0,
                "hydra_requests_used": 7,
            },
            "systems": ["x86_64-linux"],
            "scope": {"kind": "jobs", "jobs": ["appA.x86_64-linux", "divergentB.x86_64-linux"]},
            "mkdwarfs_version": "mkdwarfs (v0.12.4)",
        });

        TestInputs {
            manifest,
            eval_errors,
            aggregates,
            adjacency,
            outcomes,
            fidelity,
            provenance,
            relay_substituters: vec!["https://cache.nixos.org".to_string()],
            truth_swept_at: "2026-05-28T12:00:00Z".parse().unwrap(),
            drv_text_overrides,
            recipe_digest,
            some_unit_drv: APP_A_DRV.to_string(),
        }
    }

    #[tokio::test]
    async fn staged_archive_round_trips_through_the_reader() {
        let tmp = tempfile::tempdir().unwrap();
        let inputs = test_inputs();
        let staged = stage_archive(&tmp.path().join("archive"), &inputs.stage())
            .await
            .unwrap();
        assert_eq!(staged.archive_id.len(), 64);
        assert_eq!(staged.archive_id_short, staged.archive_id[..16]);

        let archive = ReplayArchive::open(&tmp.path().join("archive")).unwrap();
        let m = archive.manifest();
        // capabilities written by this recorder
        assert!(!m.capabilities.timed);
        assert!(m.capabilities.expected_outcomes);
        assert!(m.capabilities.output_hashes);
        assert!(m.capabilities.dependency_closures);
        assert!(m.capabilities.impure_env);
        assert!(!m.capabilities.embedded_store_paths);
        assert!(!m.fat);
        assert_eq!(m.from, m.to, "timeless archive");
        assert_eq!(
            m.substituters.relay,
            vec!["https://cache.nixos.org".to_string()]
        );
        // counts line up with the inputs
        assert_eq!(m.counts.requests, 2);
        assert_eq!(m.counts.workload_units, 2);
        assert_eq!(m.counts.expected_outcomes, 2);
        assert_eq!(m.counts.embedded_drvs, 3);
        assert_eq!(m.counts.embedded_store_paths, 0);
        // provenance audit fields survive verbatim
        assert_eq!(m.provenance["recorder"], "rio-parity-eval");
        assert_eq!(m.provenance["recipe_digest"], inputs.recipe_digest);
        assert_eq!(m.provenance["source"]["hydra_eval_id"], 1824219);
        assert_eq!(m.provenance["fidelity"]["divergent"], false);
        assert!(
            m.provenance["stats"]["hydra_requests_used"]
                .as_u64()
                .is_some()
        );
        // members: one synthesized timeless request per unit
        let reqs = archive.requests();
        assert_eq!(reqs.len(), 2);
        assert!(
            reqs.iter().all(|r| r.session == 0
                && r.targets.len() == 1
                && r.targets[0].outputs == vec!["*"])
        );
        // units carry label/system/outputs/required_features and the divergent flag
        let units = archive.units();
        assert_eq!(units.len(), 2);
        assert!(units.values().any(|u| u.identity_divergent));
        let app_a = &units[APP_A_DRV];
        assert_eq!(app_a.label.as_deref(), Some("appA.x86_64-linux"));
        assert_eq!(app_a.system.as_deref(), Some("x86_64-linux"));
        assert_eq!(app_a.outputs["out"], APP_A_OUT);
        assert_eq!(app_a.required_features, vec!["big-parallel".to_string()]);
        assert!(!app_a.identity_divergent);
        // closures: one record per closure drv, adjacency form
        assert_eq!(archive.closures().len(), 3);
        // exclusions: eval error + aggregate with the fixed reasons
        let excl = archive.exclusions();
        assert_eq!(excl.len(), 2);
        assert!(excl.iter().any(|e| e.reason == "eval-error"));
        assert!(excl.iter().any(|e| e.reason == "aggregate"));
        // outcomes parse and the built record carries hashes
        let outs = archive.outcomes();
        assert_eq!(outs.len(), 2);
        let built = archive.expected_outcome(0, APP_A_DRV).unwrap();
        assert_eq!(built.outcome, ExpectedOutcome::Built);
        assert_eq!(built.outputs["out"].nar_size, 226_504);
        // impure-env restricted to workload units: the dependency-only drv
        // never reaches the member.
        assert_eq!(
            archive.impure_env().keys().collect::<Vec<_>>(),
            vec![APP_A_DRV]
        );
        // drv ATerm members are readable back
        assert!(
            archive
                .read_drv(&inputs.some_unit_drv)
                .unwrap()
                .contains("Derive(")
        );
        // fidelity.json is present as an extra (ignored) member next to the
        // known members
        assert!(tmp.path().join("archive/fidelity.json").exists());
    }

    #[tokio::test]
    async fn provenance_must_be_a_json_object() {
        let tmp = tempfile::tempdir().unwrap();
        let inputs = test_inputs();
        let mut stage = inputs.stage();
        stage.provenance = serde_json::Value::String("not an object".to_string());
        let err = stage_archive(&tmp.path().join("archive"), &stage)
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("must be a JSON object"), "got: {err}");
    }

    #[tokio::test]
    async fn missing_closure_drv_text_is_a_hard_error_naming_the_path() {
        let tmp = tempfile::tempdir().unwrap();
        let mut inputs = test_inputs();
        // Drop the dependency's override: the staging builder then tries the
        // real store path, which does not exist on the test host.
        inputs.drv_text_overrides.remove(DEP_DRV);
        let err = format!(
            "{:#}",
            stage_archive(&tmp.path().join("archive"), &inputs.stage())
                .await
                .unwrap_err()
        );
        assert!(err.contains(DEP_DRV), "got: {err}");
        assert!(err.contains("local store"), "got: {err}");
    }

    #[test]
    fn mkdwarfs_version_banner_parses_both_output_shapes() {
        // Releases that support `--version` print the version as the first
        // output line.
        assert_eq!(
            parse_mkdwarfs_version_banner("mkdwarfs (v0.12.4)\nbuilt for x86_64 Linux\n"),
            Some("mkdwarfs (v0.12.4)".to_string())
        );
        // dwarfs 0.14.0 rejects `--version`; its `--help` banner carries the
        // version line below the ASCII-art header.
        let help_banner = r#"     ___                  ___ ___
    |   \__ __ ____ _ _ _| __/ __|         Deduplicating Warp-speed
    | |) \ V  V / _` | '_| _|\__ \      Advanced Read-only File System
    |___/ \_/\_/\__,_|_| |_| |___/         by Marcus Holland-Moritz

mkdwarfs (v0.14.0)
built for x86_64 Linux using GNU 15.2.0

Usage: mkdwarfs [OPTIONS...]
"#;
        assert_eq!(
            parse_mkdwarfs_version_banner(help_banner),
            Some("mkdwarfs (v0.14.0)".to_string())
        );
        // Output with no banner line at all (the 0.14.0 `--version` error)
        // yields nothing instead of a fabricated version.
        assert_eq!(
            parse_mkdwarfs_version_banner("error: unrecognised option '--version'\n"),
            None
        );
        assert_eq!(parse_mkdwarfs_version_banner(""), None);
    }
}
