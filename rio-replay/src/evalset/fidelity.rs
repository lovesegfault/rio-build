//! drvPath fidelity gate for locally produced eval sets.
//!
//! An eval set is only trustworthy if the local evaluation produced the
//! exact derivations Hydra built, so each manifest record's drvPath is
//! compared against Hydra's reported drvpath for the same job. Scoped
//! sets compare every in-scope job (exhaustive mode); full-evaluation
//! sets compare a bounded sample of jobs (sampled mode). Only a drvPath
//! mismatch marks the set divergent; jobs present on one side but not
//! the other are reported as coverage gaps and do not gate
//! individually. Zero coverage is different: a comparison that joined
//! no jobs at all verified nothing, so [`FidelityReport::verdict`]
//! reduces the counters to a [`FidelityVerdict`] whose passing arm
//! structurally requires a nonzero number of compared jobs — absence of
//! counter-evidence alone can never read as success. The resulting
//! report is written verbatim as `fidelity.json`.

use std::collections::{BTreeMap, BTreeSet};
use std::num::NonZeroUsize;

use serde::Serialize;

use crate::evalset::evaluator::ManifestRecord;

/// [`FidelityReport::mode`] value when every in-scope job was compared.
pub const MODE_EXHAUSTIVE: &str = "exhaustive";
/// [`FidelityReport::mode`] value when only a bounded sample of jobs
/// was compared.
pub const MODE_SAMPLED: &str = "sampled";

/// One job whose locally evaluated drvPath differs from Hydra's.
#[derive(Debug, Clone, Serialize)]
pub struct FidelityMismatch {
    pub job: String,
    pub local_drv: String,
    pub hydra_drv: String,
}

/// Contents of `fidelity.json`.
#[derive(Debug, Clone, Serialize)]
pub struct FidelityReport {
    /// [`MODE_EXHAUSTIVE`] when every in-scope job was compared (scoped
    /// sets) or [`MODE_SAMPLED`] when only a bounded sample of jobs was
    /// compared (full-evaluation sets).
    pub mode: String,
    /// Jobs compared (present on both sides).
    pub checked: usize,
    pub matched: usize,
    pub mismatches: Vec<FidelityMismatch>,
    /// Hydra ground-truth jobs with no local manifest record (coverage flag).
    pub missing_locally: Vec<String>,
    /// Local manifest jobs with no Hydra ground truth (coverage flag).
    pub missing_on_hydra: Vec<String>,
    /// True iff any drvPath mismatch was found. Gate callers must branch
    /// on [`FidelityReport::verdict`] — which also fails a zero-coverage
    /// comparison closed — never on this flag alone: an empty join has
    /// no mismatches yet verified nothing.
    pub divergent: bool,
}

/// Gate verdict of one fidelity comparison: [`FidelityReport`]'s
/// counters reduced to the states a gate must distinguish.
///
/// `Passed` carries its coverage witness — the nonzero number of jobs
/// actually compared — so the type cannot express "passed having
/// verified nothing". A comparison that joined zero jobs while jobs
/// existed on either side is its own [`FidelityVerdict::Vacuous`] state,
/// and gates must fail closed on it: an empty join means the trust
/// mechanism never ran (the classic cause being a job-name format skew
/// between the two sides), not that the eval set is faithful.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FidelityVerdict {
    /// At least one compared job's local drvPath differs from ground
    /// truth.
    Divergent { mismatches: NonZeroUsize },
    /// Every compared job matched, witnessed by `checked` actual
    /// comparisons.
    Passed { checked: NonZeroUsize },
    /// Jobs existed on at least one side but zero were compared:
    /// nothing was verified, which must read as failure, not success.
    Vacuous {
        /// Ground-truth jobs, none of which were compared.
        hydra_jobs: usize,
        /// Locally evaluated jobs, none of which were compared.
        local_jobs: usize,
    },
    /// Both sides were empty: there was genuinely nothing to verify, so
    /// the empty comparison is clean rather than vacuous.
    NothingInScope,
}

impl FidelityReport {
    /// Reduce the report to its [`FidelityVerdict`].
    ///
    /// Mismatches always dominate (a mismatch is a real comparison, so
    /// `Divergent` and `Vacuous` are mutually exclusive); a clean report
    /// passes only with `checked > 0`; with zero comparisons the gap
    /// lists are the per-side universes, distinguishing "jobs existed
    /// but the join missed all of them" (vacuous) from "nothing was in
    /// scope at all".
    pub fn verdict(&self) -> FidelityVerdict {
        if let Some(mismatches) = NonZeroUsize::new(self.mismatches.len()) {
            return FidelityVerdict::Divergent { mismatches };
        }
        if let Some(checked) = NonZeroUsize::new(self.checked) {
            return FidelityVerdict::Passed { checked };
        }
        let hydra_jobs = self.missing_locally.len();
        let local_jobs = self.missing_on_hydra.len();
        if hydra_jobs == 0 && local_jobs == 0 {
            FidelityVerdict::NothingInScope
        } else {
            FidelityVerdict::Vacuous {
                hydra_jobs,
                local_jobs,
            }
        }
    }
}

/// Compare manifest drvPaths against Hydra ground truth (job → drvpath).
///
/// Both inputs must already be restricted to the comparison scope — the
/// caller does any sampling — so a sampled Hydra map is never compared
/// against a full manifest (the asymmetry would only inflate the
/// coverage-gap lists, never fabricate a mismatch).
pub fn compare_drv_paths(
    manifest: &[ManifestRecord],
    hydra: &BTreeMap<String, String>,
    mode: &str,
) -> FidelityReport {
    let mut matched = 0usize;
    let mut checked = 0usize;
    let mut mismatches = Vec::new();
    let mut missing_on_hydra = Vec::new();
    let local_jobs: BTreeSet<&str> = manifest.iter().map(|r| r.job.as_str()).collect();

    for rec in manifest {
        match hydra.get(&rec.job) {
            Some(hydra_drv) => {
                checked += 1;
                if hydra_drv == &rec.drv_path {
                    matched += 1;
                } else {
                    mismatches.push(FidelityMismatch {
                        job: rec.job.clone(),
                        local_drv: rec.drv_path.clone(),
                        hydra_drv: hydra_drv.clone(),
                    });
                }
            }
            None => missing_on_hydra.push(rec.job.clone()),
        }
    }
    let missing_locally: Vec<String> = hydra
        .keys()
        .filter(|job| !local_jobs.contains(job.as_str()))
        .cloned()
        .collect();

    let divergent = !mismatches.is_empty();
    FidelityReport {
        mode: mode.to_string(),
        checked,
        matched,
        mismatches,
        missing_locally,
        missing_on_hydra,
        divergent,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::evalset::evaluator::ManifestRecord;
    use std::collections::BTreeMap;

    fn rec(job: &str, drv: &str) -> ManifestRecord {
        ManifestRecord {
            job: job.into(),
            system: "x86_64-linux".into(),
            attr: job.into(),
            drv_path: drv.into(),
            outputs: BTreeMap::new(),
            required_features: None,
        }
    }

    /// Ground truth straight from the recorded Hydra job fixture, so a
    /// matching manifest entry is the same check the live gate does.
    fn hydra_truth() -> BTreeMap<String, String> {
        let b: crate::hydra::HydraBuild = serde_json::from_str(
            &std::fs::read_to_string(
                crate::test_manifest_dir()
                    .join("tests/fixtures/hydra/job-nixpkgs.hello.x86_64-linux.json"),
            )
            .unwrap(),
        )
        .unwrap();
        BTreeMap::from([(b.job.clone(), b.drvpath.clone())])
    }

    #[test]
    fn matching_drvpaths_are_not_divergent() {
        let manifest = vec![rec(
            "nixpkgs.hello.x86_64-linux",
            "/nix/store/7mdg60drrnh0wq1j8hmmbhll47czm107-hello-2.12.3.drv",
        )];
        let report = compare_drv_paths(&manifest, &hydra_truth(), MODE_EXHAUSTIVE);
        assert_eq!(report.checked, 1);
        assert_eq!(report.matched, 1);
        assert!(report.mismatches.is_empty());
        assert!(!report.divergent);
        // The pass carries its coverage witness.
        assert_eq!(
            report.verdict(),
            FidelityVerdict::Passed {
                checked: NonZeroUsize::new(1).unwrap()
            }
        );
    }

    #[test]
    fn any_mismatch_marks_the_set_divergent() {
        let manifest = vec![rec(
            "nixpkgs.hello.x86_64-linux",
            "/nix/store/0000000000000000000000000000000-hello-2.12.3.drv",
        )];
        let report = compare_drv_paths(&manifest, &hydra_truth(), MODE_EXHAUSTIVE);
        assert_eq!(report.matched, 0);
        assert_eq!(report.mismatches.len(), 1);
        assert_eq!(report.mismatches[0].job, "nixpkgs.hello.x86_64-linux");
        assert_eq!(
            report.mismatches[0].hydra_drv,
            "/nix/store/7mdg60drrnh0wq1j8hmmbhll47czm107-hello-2.12.3.drv"
        );
        assert!(report.divergent);
        assert_eq!(
            report.verdict(),
            FidelityVerdict::Divergent {
                mismatches: NonZeroUsize::new(1).unwrap()
            }
        );
    }

    #[test]
    fn coverage_gaps_flag_but_do_not_gate() {
        // A local job Hydra doesn't know plus a Hydra job we didn't
        // evaluate: both recorded as gaps, neither marks divergence nor
        // spoils the pass — as long as at least one job WAS compared,
        // only a drvPath mismatch gates.
        let manifest = vec![
            rec(
                "nixpkgs.hello.x86_64-linux",
                "/nix/store/7mdg60drrnh0wq1j8hmmbhll47czm107-hello-2.12.3.drv",
            ),
            rec("nixpkgs.onlylocal.x86_64-linux", "/nix/store/aaa-x.drv"),
        ];
        let mut truth = hydra_truth();
        truth.insert(
            "nixpkgs.onlyhydra.x86_64-linux".into(),
            "/nix/store/bbb-y.drv".into(),
        );
        let report = compare_drv_paths(&manifest, &truth, MODE_EXHAUSTIVE);
        assert_eq!(report.checked, 1);
        assert_eq!(
            report.missing_on_hydra,
            vec!["nixpkgs.onlylocal.x86_64-linux"]
        );
        assert_eq!(
            report.missing_locally,
            vec!["nixpkgs.onlyhydra.x86_64-linux"]
        );
        assert!(!report.divergent);
        assert_eq!(
            report.verdict(),
            FidelityVerdict::Passed {
                checked: NonZeroUsize::new(1).unwrap()
            }
        );
    }

    #[test]
    fn zero_coverage_with_jobs_in_scope_is_vacuous_not_passed() {
        // The two sides share no job name at all (the historical cause:
        // one side's names carry display quoting). No mismatch can ever
        // accumulate, so a mismatch-only gate would read this as a pass
        // — the verdict instead fails it closed as vacuous.
        let manifest = vec![rec(
            "nixpkgs.onlylocal.x86_64-linux",
            "/nix/store/aaa-x.drv",
        )];
        let report = compare_drv_paths(&manifest, &hydra_truth(), MODE_EXHAUSTIVE);
        assert_eq!(report.checked, 0);
        assert!(
            !report.divergent,
            "a coverage gap is not drvPath divergence"
        );
        assert_eq!(
            report.verdict(),
            FidelityVerdict::Vacuous {
                hydra_jobs: 1,
                local_jobs: 1
            }
        );

        // One-sided emptiness is vacuous too: jobs existed to verify.
        let report = compare_drv_paths(&[], &hydra_truth(), MODE_EXHAUSTIVE);
        assert_eq!(
            report.verdict(),
            FidelityVerdict::Vacuous {
                hydra_jobs: 1,
                local_jobs: 0
            }
        );
        let report = compare_drv_paths(&manifest, &BTreeMap::new(), MODE_EXHAUSTIVE);
        assert_eq!(
            report.verdict(),
            FidelityVerdict::Vacuous {
                hydra_jobs: 0,
                local_jobs: 1
            }
        );
    }

    #[test]
    fn genuinely_empty_scope_is_nothing_in_scope_not_vacuous() {
        // Nothing on either side: there was nothing to verify, so the
        // empty comparison is clean — vacuity requires that jobs existed
        // and were missed.
        let report = compare_drv_paths(&[], &BTreeMap::new(), MODE_EXHAUSTIVE);
        assert_eq!(report.checked, 0);
        assert!(!report.divergent);
        assert_eq!(report.verdict(), FidelityVerdict::NothingInScope);
    }

    #[test]
    fn report_round_trips_through_fidelity_json() {
        use crate::evalset::artifacts::{EvalSetDir, FIDELITY_FILE};

        let manifest = vec![
            rec(
                "nixpkgs.hello.x86_64-linux",
                "/nix/store/0000000000000000000000000000000-hello-2.12.3.drv",
            ),
            rec("nixpkgs.onlylocal.x86_64-linux", "/nix/store/aaa-x.drv"),
        ];
        let report = compare_drv_paths(&manifest, &hydra_truth(), MODE_SAMPLED);

        let tmp = tempfile::tempdir().unwrap();
        let dir = EvalSetDir::create(tmp.path()).unwrap();
        let path = dir.write_json(FIDELITY_FILE, &report).unwrap();
        let back: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();

        assert_eq!(back["mode"], "sampled");
        assert_eq!(back["checked"], 1);
        assert_eq!(back["matched"], 0);
        assert_eq!(back["divergent"], true);
        assert_eq!(back["mismatches"][0]["job"], "nixpkgs.hello.x86_64-linux");
        assert_eq!(
            back["mismatches"][0]["local_drv"],
            "/nix/store/0000000000000000000000000000000-hello-2.12.3.drv"
        );
        assert_eq!(
            back["mismatches"][0]["hydra_drv"],
            "/nix/store/7mdg60drrnh0wq1j8hmmbhll47czm107-hello-2.12.3.drv"
        );
        assert_eq!(
            back["missing_on_hydra"][0],
            "nixpkgs.onlylocal.x86_64-linux"
        );
        assert_eq!(back["missing_locally"], serde_json::json!([]));
    }
}
