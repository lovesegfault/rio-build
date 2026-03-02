//! Nix ATerm derivation (`.drv`) parser and serializer.
//!
//! A `.drv` file is an ATerm representation of a Nix derivation containing:
//! - `outputs`: output name → (path, hashAlgo, hash) tuples
//! - `inputDrvs`: derivation path → output names (DAG edges)
//! - `inputSrcs`: source store paths
//! - `platform`, `builder`, `args`, `env`
//!
//! Format: `Derive([outputs],[inputDrvs],[inputSrcs],"platform","builder",[args],[env])`

use std::collections::{BTreeMap, BTreeSet};

use thiserror::Error;

mod aterm;
mod hash;

pub use hash::hash_derivation_modulo;

/// Errors from parsing or hashing ATerm derivations.
#[derive(Debug, Error)]
pub enum DerivationError {
    #[error("unexpected end of input")]
    UnexpectedEof,

    #[error("expected '{expected}', got '{got}'")]
    Expected { expected: String, got: String },

    #[error("invalid escape sequence: \\{0}")]
    InvalidEscape(char),

    #[error("expected '\"' to start string")]
    ExpectedStringStart,

    #[error("empty output name at index {0}")]
    EmptyOutputName(usize),

    #[error("collection too large: {0} items (max {MAX_ATERM_LIST_ITEMS})")]
    CollectionTooLarge(usize),

    #[error("input derivation not found: {0}")]
    InputNotFound(String),

    #[error("cycle detected in derivation graph at: {0}")]
    CycleDetected(String),

    #[error("derivation graph too deep at '{0}' (max {MAX_HASH_RECURSION_DEPTH} levels)")]
    RecursionLimitExceeded(String),
}

/// Maximum recursion depth for `hash_derivation_modulo` (DoS prevention).
const MAX_HASH_RECURSION_DEPTH: usize = 512;

/// Maximum number of items in any ATerm list (DoS prevention).
const MAX_ATERM_LIST_ITEMS: usize = 1_048_576;

/// A single derivation output.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DerivationOutput {
    /// Output name (e.g., "out", "dev", "lib").
    name: String,
    /// Store path for this output.
    path: String,
    /// Hash algorithm for fixed-output derivations (empty for input-addressed).
    hash_algo: String,
    /// Expected hash for fixed-output derivations (empty for input-addressed).
    hash: String,
}

impl DerivationOutput {
    /// Create a new derivation output.
    ///
    /// Returns an error if `name` is empty.
    pub fn new(
        name: impl Into<String>,
        path: impl Into<String>,
        hash_algo: impl Into<String>,
        hash: impl Into<String>,
    ) -> Result<Self, DerivationError> {
        let name = name.into();
        if name.is_empty() {
            return Err(DerivationError::EmptyOutputName(0));
        }
        Ok(DerivationOutput {
            name,
            path: path.into(),
            hash_algo: hash_algo.into(),
            hash: hash.into(),
        })
    }

    /// The output name.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// The output store path.
    pub fn path(&self) -> &str {
        &self.path
    }

    /// Hash algorithm (empty for input-addressed).
    pub fn hash_algo(&self) -> &str {
        &self.hash_algo
    }

    /// Expected hash (empty for input-addressed).
    pub fn hash(&self) -> &str {
        &self.hash
    }

    /// Whether this is a fixed-output derivation output.
    pub fn is_fixed_output(&self) -> bool {
        !self.hash_algo.is_empty()
    }
}

/// A full Nix derivation parsed from a `.drv` file.
///
/// Contains `input_drvs` (dependency DAG edges) which are NOT present in the
/// wire format's `BasicDerivation`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Derivation {
    /// Output definitions.
    outputs: Vec<DerivationOutput>,
    /// Input derivation dependencies: drv_path → set of output names.
    input_drvs: BTreeMap<String, BTreeSet<String>>,
    /// Input source store paths.
    input_srcs: BTreeSet<String>,
    /// Build platform (e.g., "x86_64-linux").
    platform: String,
    /// Builder executable path.
    builder: String,
    /// Builder arguments.
    args: Vec<String>,
    /// Environment variables.
    env: BTreeMap<String, String>,
}

impl Derivation {
    /// Convert to a `BasicDerivation` by stripping `input_drvs`.
    ///
    /// Cannot fail since a valid `Derivation` always has at least one output.
    #[cfg(test)]
    pub fn to_basic(&self) -> BasicDerivation {
        BasicDerivation::new(
            self.outputs.clone(),
            self.input_srcs.clone(),
            self.platform.clone(),
            self.builder.clone(),
            self.args.clone(),
            self.env.clone(),
        )
        .expect("Derivation always has outputs")
    }

    /// The derivation outputs.
    pub fn outputs(&self) -> &[DerivationOutput] {
        &self.outputs
    }

    /// Input derivation dependencies.
    pub fn input_drvs(&self) -> &BTreeMap<String, BTreeSet<String>> {
        &self.input_drvs
    }

    /// Input source store paths.
    pub fn input_srcs(&self) -> &BTreeSet<String> {
        &self.input_srcs
    }

    /// Build platform.
    pub fn platform(&self) -> &str {
        &self.platform
    }

    /// Builder executable path.
    pub fn builder(&self) -> &str {
        &self.builder
    }

    /// Builder arguments.
    pub fn args(&self) -> &[String] {
        &self.args
    }

    /// Environment variables.
    pub fn env(&self) -> &BTreeMap<String, String> {
        &self.env
    }

    /// Whether this is a fixed-output derivation.
    ///
    /// A FOD has exactly one output named "out" with both `hash_algo` and `hash`
    /// set (e.g., `sha256` / `r:sha256` and a hex digest).
    pub fn is_fixed_output(&self) -> bool {
        self.outputs.len() == 1
            && self.outputs[0].name() == "out"
            && !self.outputs[0].hash_algo().is_empty()
            && !self.outputs[0].hash().is_empty()
    }

    /// Whether any output is CA floating (`hash_algo` set, `hash` empty).
    ///
    /// Impure derivations also match this pattern and follow the same
    /// `hashDerivationModulo` code path (output masking).
    pub fn has_ca_floating_outputs(&self) -> bool {
        self.outputs
            .iter()
            .any(|o| !o.hash_algo().is_empty() && o.hash().is_empty())
    }
}

/// A derivation without `input_drvs` — the wire format for `wopBuildDerivation`.
///
/// This is the subset of a full [`Derivation`] that Nix sends inline in the
/// `wopBuildDerivation` opcode. The `inputDrvs` field is omitted because
/// the client uploads `.drv` files separately.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BasicDerivation {
    /// Output definitions.
    outputs: Vec<DerivationOutput>,
    /// Input source store paths.
    input_srcs: BTreeSet<String>,
    /// Build platform.
    platform: String,
    /// Builder executable path.
    builder: String,
    /// Builder arguments.
    args: Vec<String>,
    /// Environment variables.
    env: BTreeMap<String, String>,
}

impl BasicDerivation {
    /// Create a new basic derivation.
    ///
    /// Returns an error if `outputs` is empty (Nix derivations must have
    /// at least one output).
    pub fn new(
        outputs: Vec<DerivationOutput>,
        input_srcs: BTreeSet<String>,
        platform: String,
        builder: String,
        args: Vec<String>,
        env: BTreeMap<String, String>,
    ) -> Result<Self, DerivationError> {
        if outputs.is_empty() {
            return Err(DerivationError::EmptyOutputName(0));
        }
        Ok(BasicDerivation {
            outputs,
            input_srcs,
            platform,
            builder,
            args,
            env,
        })
    }

    /// The derivation outputs.
    pub fn outputs(&self) -> &[DerivationOutput] {
        &self.outputs
    }

    /// Input source store paths.
    pub fn input_srcs(&self) -> &BTreeSet<String> {
        &self.input_srcs
    }

    /// Build platform.
    pub fn platform(&self) -> &str {
        &self.platform
    }

    /// Builder executable path.
    pub fn builder(&self) -> &str {
        &self.builder
    }

    /// Builder arguments.
    pub fn args(&self) -> &[String] {
        &self.args
    }

    /// Environment variables.
    pub fn env(&self) -> &BTreeMap<String, String> {
        &self.env
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn to_basic_strips_input_drvs() {
        let aterm = r#"Derive([("out","/nix/store/abc-hello","","")],[("/nix/store/abc-bash.drv",["out"])],["/nix/store/abc-source.sh"],"x86_64-linux","/bin/bash",["-e","script.sh"],[("name","hello"),("system","x86_64-linux")])"#;

        let drv = Derivation::parse(aterm).unwrap();
        let basic = drv.to_basic();

        assert_eq!(basic.outputs().len(), 1);
        assert_eq!(basic.outputs()[0].name(), "out");
        assert_eq!(basic.input_srcs().len(), 1);
        assert_eq!(basic.platform(), "x86_64-linux");
        assert_eq!(basic.builder(), "/bin/bash");
        assert_eq!(basic.args(), &["-e", "script.sh"]);
        assert_eq!(basic.env().get("name").unwrap(), "hello");
    }

    #[test]
    fn basic_derivation_rejects_empty_outputs() {
        let result = BasicDerivation::new(
            vec![],
            std::collections::BTreeSet::new(),
            "x86_64-linux".to_string(),
            "/bin/sh".to_string(),
            vec![],
            std::collections::BTreeMap::new(),
        );
        assert!(result.is_err());
    }
}
