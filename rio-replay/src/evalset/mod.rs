//! Evaluation recording.
//!
//! Everything needed to reproduce a Hydra evaluation locally and record
//! it as a v1 replay archive: the eval recipe, the evaluator run, the
//! drvPath fidelity gate, closure-adjacency extraction, expected-outcome
//! mapping, archive staging, and the recipe key.

pub mod artifacts;
pub mod depclosure;
pub mod evaluator;
pub mod fidelity;
pub mod key;
pub mod outcomes;
pub mod package;
pub mod recipe;

/// What part of the Hydra evaluation a recording covers.
///
/// The serde encoding (kebab-case `kind` tag, snake_case fields) feeds
/// [`key::EvalSetKey::digest`] and is embedded verbatim in the archive
/// provenance (`recipe`/`scope`), so changing it changes every recipe
/// digest — treat it as frozen (the digest's golden test pins it).
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
