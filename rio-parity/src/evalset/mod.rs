//! Eval-set construction.
//!
//! Everything needed to reproduce a Hydra evaluation locally and
//! package the result: the eval recipe, the evaluator run, the drvPath
//! fidelity gate, dependency-closure enumeration, the derivation
//! archive, and the eval-set key/metadata.

pub mod artifacts;
pub mod evaluator;
pub mod fidelity;
pub mod recipe;

/// What part of the Hydra evaluation an eval set covers.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "kind", rename_all = "kebab-case")]
pub enum Scope {
    /// Every job in the evaluation (full nix-eval-jobs run).
    Full,
    /// The constituents of one aggregate job (e.g. `tested`).
    Constituents { aggregate_job: String },
    /// An explicit job list.
    Jobs { jobs: Vec<String> },
}
