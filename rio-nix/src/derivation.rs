//! Nix ATerm derivation (`.drv`) parser and serializer.
//!
//! A `.drv` file is an ATerm representation of a Nix derivation containing:
//! - `outputs`: output name → (path, hashAlgo, hash) tuples
//! - `inputDrvs`: derivation path → output names (DAG edges)
//! - `inputSrcs`: source store paths
//! - `platform`, `builder`, `args`, `env`
//!
//! Format: `Derive([outputs],[inputDrvs],[inputSrcs],"platform","builder",[args],[env])`

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};

use sha2::{Digest, Sha256};
use thiserror::Error;

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
/// wire format's `BasicDerivation`. Use [`Derivation::to_basic`] to strip them.
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
    /// Parse a `.drv` file's ATerm content.
    pub fn parse(input: &str) -> Result<Self, DerivationError> {
        let mut parser = ATermParser::new(input);
        parser.parse_derivation()
    }

    /// Serialize back to ATerm format.
    pub fn to_aterm(&self) -> String {
        let mut out = String::new();
        out.push_str("Derive(");

        // outputs
        out.push('[');
        for (i, o) in self.outputs.iter().enumerate() {
            if i > 0 {
                out.push(',');
            }
            out.push('(');
            write_aterm_string(&mut out, &o.name);
            out.push(',');
            write_aterm_string(&mut out, &o.path);
            out.push(',');
            write_aterm_string(&mut out, &o.hash_algo);
            out.push(',');
            write_aterm_string(&mut out, &o.hash);
            out.push(')');
        }
        out.push(']');
        out.push(',');

        // inputDrvs
        out.push('[');
        for (i, (drv_path, output_names)) in self.input_drvs.iter().enumerate() {
            if i > 0 {
                out.push(',');
            }
            out.push('(');
            write_aterm_string(&mut out, drv_path);
            out.push(',');
            out.push('[');
            for (j, name) in output_names.iter().enumerate() {
                if j > 0 {
                    out.push(',');
                }
                write_aterm_string(&mut out, name);
            }
            out.push(']');
            out.push(')');
        }
        out.push(']');
        out.push(',');

        // inputSrcs
        out.push('[');
        for (i, src) in self.input_srcs.iter().enumerate() {
            if i > 0 {
                out.push(',');
            }
            write_aterm_string(&mut out, src);
        }
        out.push(']');
        out.push(',');

        // platform, builder
        write_aterm_string(&mut out, &self.platform);
        out.push(',');
        write_aterm_string(&mut out, &self.builder);
        out.push(',');

        // args
        out.push('[');
        for (i, arg) in self.args.iter().enumerate() {
            if i > 0 {
                out.push(',');
            }
            write_aterm_string(&mut out, arg);
        }
        out.push(']');
        out.push(',');

        // env
        out.push('[');
        for (i, (key, value)) in self.env.iter().enumerate() {
            if i > 0 {
                out.push(',');
            }
            out.push('(');
            write_aterm_string(&mut out, key);
            out.push(',');
            write_aterm_string(&mut out, value);
            out.push(')');
        }
        out.push(']');

        out.push(')');
        out
    }

    /// Serialize to ATerm with modified `inputDrvs` keys and optional output masking.
    ///
    /// Used by `hash_derivation_modulo` to produce the canonical ATerm for hashing.
    ///
    /// - `input_rewrites`: maps original drv path → replacement string (hex hash).
    ///   Every key in `self.input_drvs` must be present; missing keys return
    ///   `InputNotFound`.
    /// - `mask_outputs`: if true, output paths are replaced with `""` (for CA
    ///   floating / impure derivations).
    ///
    /// The `inputDrvs` section is re-sorted by the *replacement* keys (hex hashes),
    /// matching Nix C++ `Derivation::unparse` with `actualInputs`.
    pub fn to_aterm_modulo(
        &self,
        input_rewrites: &BTreeMap<String, String>,
        mask_outputs: bool,
    ) -> Result<String, DerivationError> {
        let mut out = String::new();
        out.push_str("Derive(");

        // outputs (optionally mask paths)
        out.push('[');
        for (i, o) in self.outputs.iter().enumerate() {
            if i > 0 {
                out.push(',');
            }
            out.push('(');
            write_aterm_string(&mut out, &o.name);
            out.push(',');
            if mask_outputs {
                write_aterm_string(&mut out, "");
            } else {
                write_aterm_string(&mut out, &o.path);
            }
            out.push(',');
            write_aterm_string(&mut out, &o.hash_algo);
            out.push(',');
            write_aterm_string(&mut out, &o.hash);
            out.push(')');
        }
        out.push(']');
        out.push(',');

        // inputDrvs -- re-sorted by replacement keys
        let mut replaced_inputs: BTreeMap<&str, &BTreeSet<String>> = BTreeMap::new();
        for (drv_path, output_names) in &self.input_drvs {
            let new_key = input_rewrites
                .get(drv_path)
                .ok_or_else(|| DerivationError::InputNotFound(drv_path.clone()))?;
            replaced_inputs.insert(new_key.as_str(), output_names);
        }

        out.push('[');
        for (i, (key, output_names)) in replaced_inputs.iter().enumerate() {
            if i > 0 {
                out.push(',');
            }
            out.push('(');
            write_aterm_string(&mut out, key);
            out.push(',');
            out.push('[');
            for (j, name) in output_names.iter().enumerate() {
                if j > 0 {
                    out.push(',');
                }
                write_aterm_string(&mut out, name);
            }
            out.push(']');
            out.push(')');
        }
        out.push(']');
        out.push(',');

        // inputSrcs, platform, builder, args, env -- identical to to_aterm()
        out.push('[');
        for (i, src) in self.input_srcs.iter().enumerate() {
            if i > 0 {
                out.push(',');
            }
            write_aterm_string(&mut out, src);
        }
        out.push(']');
        out.push(',');

        write_aterm_string(&mut out, &self.platform);
        out.push(',');
        write_aterm_string(&mut out, &self.builder);
        out.push(',');

        out.push('[');
        for (i, arg) in self.args.iter().enumerate() {
            if i > 0 {
                out.push(',');
            }
            write_aterm_string(&mut out, arg);
        }
        out.push(']');
        out.push(',');

        out.push('[');
        for (i, (key, value)) in self.env.iter().enumerate() {
            if i > 0 {
                out.push(',');
            }
            out.push('(');
            write_aterm_string(&mut out, key);
            out.push(',');
            write_aterm_string(&mut out, value);
            out.push(')');
        }
        out.push(']');

        out.push(')');
        Ok(out)
    }

    /// Convert to a `BasicDerivation` by stripping `input_drvs`.
    ///
    /// Cannot fail since a valid `Derivation` always has at least one output.
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

// ---------------------------------------------------------------------------
// hashDerivationModulo
// ---------------------------------------------------------------------------

/// Compute the modular derivation hash, matching Nix C++ `hashDerivationModulo`.
///
/// Three cases:
/// - **FOD** (fixed-output): `SHA-256("fixed:out:{hash_algo}:{hash}:")`
/// - **Input-addressed**: replace `inputDrvs` keys with recursive modular hashes,
///   then `SHA-256(modified_aterm)`
/// - **CA floating / impure**: same as input-addressed but output paths are masked
///   to `""` in the ATerm
///
/// `resolve_input` maps a drv path string (e.g. `"/nix/store/abc.drv"`) to
/// the parsed `Derivation`. All transitive inputs must be resolvable.
///
/// `hash_cache` provides memoisation across calls (keyed by drv path string).
pub fn hash_derivation_modulo<'c>(
    drv: &'c Derivation,
    drv_path: &str,
    resolve_input: &dyn Fn(&str) -> Option<&'c Derivation>,
    hash_cache: &mut HashMap<String, [u8; 32]>,
) -> Result<[u8; 32], DerivationError> {
    let mut visiting = HashSet::new();
    hash_derivation_modulo_inner(drv, drv_path, resolve_input, hash_cache, &mut visiting, 0)
}

fn hash_derivation_modulo_inner<'c>(
    drv: &'c Derivation,
    drv_path: &str,
    resolve_input: &dyn Fn(&str) -> Option<&'c Derivation>,
    hash_cache: &mut HashMap<String, [u8; 32]>,
    visiting: &mut HashSet<String>,
    depth: usize,
) -> Result<[u8; 32], DerivationError> {
    // Memoisation
    if let Some(&cached) = hash_cache.get(drv_path) {
        return Ok(cached);
    }

    // Cycle detection
    if visiting.contains(drv_path) {
        return Err(DerivationError::CycleDetected(drv_path.to_string()));
    }

    // Depth limit
    if depth >= MAX_HASH_RECURSION_DEPTH {
        return Err(DerivationError::RecursionLimitExceeded(
            drv_path.to_string(),
        ));
    }

    visiting.insert(drv_path.to_string());

    // Compute the hash, ensuring `visiting` is cleaned up on all paths
    // (error or success) to prevent false CycleDetected in sibling branches.
    let result = (|| -> Result<[u8; 32], DerivationError> {
        if drv.is_fixed_output() {
            // FOD base case: hash the fingerprint string (no recursion)
            let output = &drv.outputs()[0];
            let fingerprint = format!("fixed:out:{}:{}:", output.hash_algo(), output.hash());
            Ok(Sha256::digest(fingerprint.as_bytes()).into())
        } else {
            // Input-addressed or CA floating: recurse on input drvs
            let mask_outputs = drv.has_ca_floating_outputs();

            let mut input_rewrites: BTreeMap<String, String> = BTreeMap::new();
            for input_drv_path in drv.input_drvs().keys() {
                let input_drv = resolve_input(input_drv_path)
                    .ok_or_else(|| DerivationError::InputNotFound(input_drv_path.clone()))?;
                let input_hash = hash_derivation_modulo_inner(
                    input_drv,
                    input_drv_path,
                    resolve_input,
                    hash_cache,
                    visiting,
                    depth + 1,
                )?;
                input_rewrites.insert(input_drv_path.clone(), hex::encode(input_hash));
            }

            let modified_aterm = drv.to_aterm_modulo(&input_rewrites, mask_outputs)?;
            Ok(Sha256::digest(modified_aterm.as_bytes()).into())
        }
    })();

    visiting.remove(drv_path);

    let hash = result?;
    hash_cache.insert(drv_path.to_string(), hash);
    Ok(hash)
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

// ---------------------------------------------------------------------------
// ATerm serialization helper
// ---------------------------------------------------------------------------

/// Write an ATerm-escaped string (with surrounding quotes) to the output.
fn write_aterm_string(out: &mut String, s: &str) {
    out.push('"');
    for ch in s.chars() {
        match ch {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c => out.push(c),
        }
    }
    out.push('"');
}

// ---------------------------------------------------------------------------
// ATerm parser
// ---------------------------------------------------------------------------

struct ATermParser<'a> {
    input: &'a str,
    pos: usize,
}

impl<'a> ATermParser<'a> {
    fn new(input: &'a str) -> Self {
        ATermParser { input, pos: 0 }
    }

    /// Peek at the current character without consuming.
    fn peek(&self) -> Option<char> {
        self.input[self.pos..].chars().next()
    }

    /// Advance past the current character.
    fn advance(&mut self) {
        if let Some(ch) = self.input[self.pos..].chars().next() {
            self.pos += ch.len_utf8();
        }
    }

    /// Consume the expected literal string, or return an error.
    fn expect(&mut self, expected: &str) -> Result<(), DerivationError> {
        if self.input[self.pos..].starts_with(expected) {
            self.pos += expected.len();
            Ok(())
        } else {
            let remaining = &self.input[self.pos..];
            let got: String = remaining.chars().take(expected.len().max(10)).collect();
            Err(DerivationError::Expected {
                expected: expected.to_string(),
                got,
            })
        }
    }

    /// Parse an ATerm-quoted string: `"..."` with escape handling.
    fn parse_string(&mut self) -> Result<String, DerivationError> {
        if self.peek() != Some('"') {
            return Err(DerivationError::ExpectedStringStart);
        }
        self.advance(); // consume opening quote

        let mut result = String::new();
        loop {
            match self.peek() {
                None => return Err(DerivationError::UnexpectedEof),
                Some('"') => {
                    self.advance(); // consume closing quote
                    return Ok(result);
                }
                Some('\\') => {
                    self.advance(); // consume backslash
                    match self.peek() {
                        None => return Err(DerivationError::UnexpectedEof),
                        Some('n') => {
                            result.push('\n');
                            self.advance();
                        }
                        Some('r') => {
                            result.push('\r');
                            self.advance();
                        }
                        Some('t') => {
                            result.push('\t');
                            self.advance();
                        }
                        Some('\\') => {
                            result.push('\\');
                            self.advance();
                        }
                        Some('"') => {
                            result.push('"');
                            self.advance();
                        }
                        Some(c) => return Err(DerivationError::InvalidEscape(c)),
                    }
                }
                Some(c) => {
                    result.push(c);
                    self.advance();
                }
            }
        }
    }

    /// Parse a comma-separated list of strings inside `[...]`.
    fn parse_string_list(&mut self) -> Result<Vec<String>, DerivationError> {
        self.expect("[")?;
        let mut items = Vec::new();
        if self.peek() == Some(']') {
            self.advance();
            return Ok(items);
        }
        loop {
            if items.len() >= MAX_ATERM_LIST_ITEMS {
                return Err(DerivationError::CollectionTooLarge(items.len()));
            }
            items.push(self.parse_string()?);
            match self.peek() {
                Some(',') => self.advance(),
                Some(']') => {
                    self.advance();
                    return Ok(items);
                }
                _ => return Err(DerivationError::UnexpectedEof),
            }
        }
    }

    /// Parse the outputs list: `[("name","path","hashAlgo","hash"), ...]`
    fn parse_outputs(&mut self) -> Result<Vec<DerivationOutput>, DerivationError> {
        self.expect("[")?;
        let mut outputs = Vec::new();
        if self.peek() == Some(']') {
            self.advance();
            return Ok(outputs);
        }
        loop {
            if outputs.len() >= MAX_ATERM_LIST_ITEMS {
                return Err(DerivationError::CollectionTooLarge(outputs.len()));
            }
            self.expect("(")?;
            let name = self.parse_string()?;
            self.expect(",")?;
            let path = self.parse_string()?;
            self.expect(",")?;
            let hash_algo = self.parse_string()?;
            self.expect(",")?;
            let hash = self.parse_string()?;
            self.expect(")")?;

            outputs.push(
                DerivationOutput::new(name, path, hash_algo, hash)
                    .map_err(|_| DerivationError::EmptyOutputName(outputs.len()))?,
            );

            match self.peek() {
                Some(',') => self.advance(),
                Some(']') => {
                    self.advance();
                    return Ok(outputs);
                }
                _ => return Err(DerivationError::UnexpectedEof),
            }
        }
    }

    /// Parse inputDrvs: `[("drvPath",["out1","out2"]), ...]`
    fn parse_input_drvs(&mut self) -> Result<BTreeMap<String, BTreeSet<String>>, DerivationError> {
        self.expect("[")?;
        let mut drvs = BTreeMap::new();
        if self.peek() == Some(']') {
            self.advance();
            return Ok(drvs);
        }
        loop {
            if drvs.len() >= MAX_ATERM_LIST_ITEMS {
                return Err(DerivationError::CollectionTooLarge(drvs.len()));
            }
            self.expect("(")?;
            let drv_path = self.parse_string()?;
            self.expect(",")?;
            let output_names: BTreeSet<String> = self.parse_string_list()?.into_iter().collect();
            self.expect(")")?;

            drvs.insert(drv_path, output_names);

            match self.peek() {
                Some(',') => self.advance(),
                Some(']') => {
                    self.advance();
                    return Ok(drvs);
                }
                _ => return Err(DerivationError::UnexpectedEof),
            }
        }
    }

    /// Parse environment: `[("key","value"), ...]`
    fn parse_env(&mut self) -> Result<BTreeMap<String, String>, DerivationError> {
        self.expect("[")?;
        let mut env = BTreeMap::new();
        if self.peek() == Some(']') {
            self.advance();
            return Ok(env);
        }
        loop {
            if env.len() >= MAX_ATERM_LIST_ITEMS {
                return Err(DerivationError::CollectionTooLarge(env.len()));
            }
            self.expect("(")?;
            let key = self.parse_string()?;
            self.expect(",")?;
            let value = self.parse_string()?;
            self.expect(")")?;

            env.insert(key, value);

            match self.peek() {
                Some(',') => self.advance(),
                Some(']') => {
                    self.advance();
                    return Ok(env);
                }
                _ => return Err(DerivationError::UnexpectedEof),
            }
        }
    }

    /// Parse a full `Derive(...)` expression.
    fn parse_derivation(&mut self) -> Result<Derivation, DerivationError> {
        self.expect("Derive(")?;

        let outputs = self.parse_outputs()?;
        self.expect(",")?;

        let input_drvs = self.parse_input_drvs()?;
        self.expect(",")?;

        let input_srcs: BTreeSet<String> = self.parse_string_list()?.into_iter().collect();
        self.expect(",")?;

        let platform = self.parse_string()?;
        self.expect(",")?;

        let builder = self.parse_string()?;
        self.expect(",")?;

        let args = self.parse_string_list()?;
        self.expect(",")?;

        let env = self.parse_env()?;

        self.expect(")")?;

        Ok(Derivation {
            outputs,
            input_drvs,
            input_srcs,
            platform,
            builder,
            args,
            env,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_simple_derivation() {
        let aterm = r#"Derive([("out","/nix/store/abc-simple-test","","")],[],[],"x86_64-linux","/bin/sh",["-c","echo hello > $out"],[("builder","/bin/sh"),("name","simple-test"),("out","/nix/store/abc-simple-test"),("system","x86_64-linux")])"#;

        let drv = Derivation::parse(aterm).unwrap();
        assert_eq!(drv.outputs().len(), 1);
        assert_eq!(drv.outputs()[0].name(), "out");
        assert_eq!(drv.outputs()[0].path(), "/nix/store/abc-simple-test");
        assert_eq!(drv.outputs()[0].hash_algo(), "");
        assert_eq!(drv.outputs()[0].hash(), "");
        assert!(!drv.outputs()[0].is_fixed_output());
        assert!(drv.input_drvs().is_empty());
        assert!(drv.input_srcs().is_empty());
        assert_eq!(drv.platform(), "x86_64-linux");
        assert_eq!(drv.builder(), "/bin/sh");
        assert_eq!(drv.args(), &["-c", "echo hello > $out"]);
        assert_eq!(drv.env().get("name").unwrap(), "simple-test");
        assert_eq!(drv.env().get("system").unwrap(), "x86_64-linux");
    }

    #[test]
    fn parse_multi_output_derivation() {
        let aterm = r#"Derive([("dev","/nix/store/abc-dev","",""),("lib","/nix/store/abc-lib","",""),("out","/nix/store/abc-out","","")],[],[],"x86_64-linux","/bin/sh",["-c","mkdir $out $dev $lib"],[("dev","/nix/store/abc-dev"),("lib","/nix/store/abc-lib"),("name","multi"),("out","/nix/store/abc-out"),("outputs","out dev lib"),("system","x86_64-linux")])"#;

        let drv = Derivation::parse(aterm).unwrap();
        assert_eq!(drv.outputs().len(), 3);
        assert_eq!(drv.outputs()[0].name(), "dev");
        assert_eq!(drv.outputs()[1].name(), "lib");
        assert_eq!(drv.outputs()[2].name(), "out");
        assert_eq!(drv.env().get("outputs").unwrap(), "out dev lib");
    }

    #[test]
    fn parse_fixed_output_derivation() {
        let aterm = r#"Derive([("out","/nix/store/abc-fixed","sha256","abcdef0123456789")],[],[],"x86_64-linux","/bin/sh",["-c","echo"],[("name","fixed"),("out","/nix/store/abc-fixed"),("outputHash","abcdef0123456789"),("outputHashAlgo","sha256"),("system","x86_64-linux")])"#;

        let drv = Derivation::parse(aterm).unwrap();
        assert_eq!(drv.outputs().len(), 1);
        assert_eq!(drv.outputs()[0].hash_algo(), "sha256");
        assert_eq!(drv.outputs()[0].hash(), "abcdef0123456789");
        assert!(drv.outputs()[0].is_fixed_output());
    }

    #[test]
    fn parse_with_input_drvs() {
        let aterm = r#"Derive([("out","/nix/store/abc-hello","","")],[("/nix/store/abc-bash.drv",["out"]),("/nix/store/abc-stdenv.drv",["out"])],["/nix/store/abc-source.sh"],"x86_64-linux","/nix/store/abc-bash/bin/bash",["-e","/nix/store/abc-source.sh"],[("name","hello"),("out","/nix/store/abc-hello"),("system","x86_64-linux")])"#;

        let drv = Derivation::parse(aterm).unwrap();
        assert_eq!(drv.input_drvs().len(), 2);
        assert!(drv.input_drvs().contains_key("/nix/store/abc-bash.drv"));
        assert!(drv.input_drvs().contains_key("/nix/store/abc-stdenv.drv"));
        let bash_outputs = drv.input_drvs().get("/nix/store/abc-bash.drv").unwrap();
        assert!(bash_outputs.contains("out"));
        assert_eq!(drv.input_srcs().len(), 1);
        assert!(drv.input_srcs().contains("/nix/store/abc-source.sh"));
    }

    #[test]
    fn parse_with_escaped_strings() {
        let aterm = r#"Derive([("out","/nix/store/abc-escape","","")],[],[],"x86_64-linux","/bin/sh",["-c","echo \"hello\\nworld\" > $out"],[("env_val","line1\nline2\ttab\nquote\"end"),("name","escape"),("out","/nix/store/abc-escape"),("system","x86_64-linux")])"#;

        let drv = Derivation::parse(aterm).unwrap();
        assert_eq!(drv.args(), &["-c", "echo \"hello\\nworld\" > $out"]);
        assert_eq!(
            drv.env().get("env_val").unwrap(),
            "line1\nline2\ttab\nquote\"end"
        );
    }

    #[test]
    fn parse_empty_env_value() {
        let aterm = r#"Derive([("out","/nix/store/abc-test","","")],[],[],"x86_64-linux","/bin/sh",[],[("empty",""),("name","test"),("out","/nix/store/abc-test"),("system","x86_64-linux")])"#;

        let drv = Derivation::parse(aterm).unwrap();
        assert_eq!(drv.env().get("empty").unwrap(), "");
    }

    #[test]
    fn roundtrip_simple() {
        let aterm = r#"Derive([("out","/nix/store/abc-simple","","")],[],[],"x86_64-linux","/bin/sh",["-c","echo hello > $out"],[("builder","/bin/sh"),("name","simple"),("out","/nix/store/abc-simple"),("system","x86_64-linux")])"#;

        let drv = Derivation::parse(aterm).unwrap();
        let serialized = drv.to_aterm();
        assert_eq!(serialized, aterm);
    }

    #[test]
    fn roundtrip_with_input_drvs() {
        let aterm = r#"Derive([("out","/nix/store/abc-hello","","")],[("/nix/store/abc-bash.drv",["out"]),("/nix/store/abc-stdenv.drv",["out"])],["/nix/store/abc-source.sh"],"x86_64-linux","/nix/store/abc-bash/bin/bash",["-e","/nix/store/abc-source.sh"],[("name","hello"),("out","/nix/store/abc-hello"),("system","x86_64-linux")])"#;

        let drv = Derivation::parse(aterm).unwrap();
        let serialized = drv.to_aterm();
        assert_eq!(serialized, aterm);
    }

    #[test]
    fn roundtrip_with_escapes() {
        let aterm = r#"Derive([("out","/nix/store/abc-escape","","")],[],[],"x86_64-linux","/bin/sh",["-c","echo \"hello\\nworld\""],[("env","line1\nline2\ttab\nquote\"end"),("name","escape"),("out","/nix/store/abc-escape"),("system","x86_64-linux")])"#;

        let drv = Derivation::parse(aterm).unwrap();
        let serialized = drv.to_aterm();
        assert_eq!(serialized, aterm);
    }

    #[test]
    fn roundtrip_fixed_output() {
        let aterm = r#"Derive([("out","/nix/store/abc-fixed","sha256","abcdef0123456789")],[],[],"x86_64-linux","/bin/sh",["-c","echo"],[("name","fixed"),("out","/nix/store/abc-fixed"),("system","x86_64-linux")])"#;

        let drv = Derivation::parse(aterm).unwrap();
        let serialized = drv.to_aterm();
        assert_eq!(serialized, aterm);
    }

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

    #[test]
    fn parse_rejects_invalid_prefix() {
        assert!(Derivation::parse("NotDerive([],...)").is_err());
    }

    #[test]
    fn parse_rejects_truncated_input() {
        assert!(Derivation::parse("Derive(").is_err());
        assert!(Derivation::parse("Derive([").is_err());
        assert!(Derivation::parse(r#"Derive([("out")"#).is_err());
    }

    #[test]
    fn parse_rejects_empty_output_name() {
        let aterm = r#"Derive([("","/nix/store/abc-test","","")],[],[],"x86_64-linux","/bin/sh",[],[("name","test"),("system","x86_64-linux")])"#;
        assert!(matches!(
            Derivation::parse(aterm),
            Err(DerivationError::EmptyOutputName(0))
        ));
    }

    #[test]
    fn parse_rejects_invalid_escape() {
        let aterm = r#"Derive([("out","/nix/store/abc-test","","")],[],[],"x86_64-linux","/bin/sh",[],[("name","tes\x")])"#;
        assert!(matches!(
            Derivation::parse(aterm),
            Err(DerivationError::InvalidEscape('x'))
        ));
    }

    #[test]
    fn parse_input_drvs_with_multiple_outputs() {
        let aterm = r#"Derive([("out","/nix/store/abc-test","","")],[("/nix/store/abc-multi.drv",["dev","lib","out"])],[],"x86_64-linux","/bin/sh",[],[("name","test"),("system","x86_64-linux")])"#;

        let drv = Derivation::parse(aterm).unwrap();
        let multi_outputs = drv.input_drvs().get("/nix/store/abc-multi.drv").unwrap();
        assert_eq!(multi_outputs.len(), 3);
        assert!(multi_outputs.contains("dev"));
        assert!(multi_outputs.contains("lib"));
        assert!(multi_outputs.contains("out"));
    }

    /// Parse a real `.drv` file from the local Nix store.
    #[test]
    fn parse_real_drv_file() {
        // Generate a simple derivation and parse it
        let output = std::process::Command::new("nix-instantiate")
            .args(["--expr", r#"derivation { name = "golden-test"; builder = "/bin/sh"; args = ["-c" "echo hello > $out"]; system = builtins.currentSystem; }"#])
            .output();

        let output = match output {
            Ok(o) if o.status.success() => o,
            _ => {
                eprintln!("skipping parse_real_drv_file: nix-instantiate not available");
                return;
            }
        };

        let drv_path = String::from_utf8(output.stdout).unwrap();
        let drv_path = drv_path.trim();

        let drv_content = std::fs::read_to_string(drv_path).unwrap();
        let drv = Derivation::parse(&drv_content).unwrap();

        assert_eq!(drv.outputs().len(), 1);
        assert_eq!(drv.outputs()[0].name(), "out");
        assert_eq!(drv.platform(), "x86_64-linux");
        assert_eq!(drv.builder(), "/bin/sh");
        assert_eq!(drv.env().get("name").unwrap(), "golden-test");

        // Verify roundtrip: parse → serialize → parse
        let serialized = drv.to_aterm();
        let reparsed = Derivation::parse(&serialized).unwrap();
        assert_eq!(drv, reparsed);
    }

    /// Parse the real hello .drv (complex, many deps).
    #[test]
    fn parse_hello_drv() {
        let output = std::process::Command::new("nix-instantiate")
            .args(["<nixpkgs>", "-A", "hello"])
            .output();

        let output = match output {
            Ok(o) if o.status.success() => o,
            _ => {
                eprintln!(
                    "skipping parse_hello_drv: nix-instantiate not available or nixpkgs not found"
                );
                return;
            }
        };

        let drv_path = String::from_utf8(output.stdout).unwrap();
        let drv_path = drv_path.trim();

        let drv_content = std::fs::read_to_string(drv_path).unwrap();
        let drv = Derivation::parse(&drv_content).unwrap();

        // hello has 1 output, multiple input drvs, and a rich env
        assert_eq!(drv.outputs().len(), 1);
        assert_eq!(drv.outputs()[0].name(), "out");
        assert!(!drv.input_drvs().is_empty());
        assert_eq!(drv.env().get("pname").unwrap(), "hello");
        assert_eq!(drv.platform(), "x86_64-linux");

        // Roundtrip
        let serialized = drv.to_aterm();
        let reparsed = Derivation::parse(&serialized).unwrap();
        assert_eq!(drv, reparsed);
    }

    mod hash_derivation_modulo_tests {
        use super::*;
        use std::collections::{BTreeMap, HashMap};

        /// Helper: create a simple input-addressed derivation (no inputDrvs).
        fn leaf_ia_drv() -> Derivation {
            Derivation::parse(
                r#"Derive([("out","/nix/store/abc-leaf","","")],[],[],"x86_64-linux","/bin/sh",["-c","echo hello"],[("name","leaf"),("out","/nix/store/abc-leaf"),("system","x86_64-linux")])"#,
            ).unwrap()
        }

        /// Helper: create a fixed-output derivation.
        fn fod_drv() -> Derivation {
            Derivation::parse(
                r#"Derive([("out","/nix/store/xyz-fixed","sha256","e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855")],[],[],"x86_64-linux","/bin/sh",["-c","echo"],[("name","fixed"),("out","/nix/store/xyz-fixed"),("outputHash","e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"),("outputHashAlgo","sha256"),("system","x86_64-linux")])"#,
            ).unwrap()
        }

        /// Helper: create an IA derivation that depends on the FOD.
        fn ia_with_fod_input() -> Derivation {
            Derivation::parse(
                r#"Derive([("out","/nix/store/def-dependent","","")],[("/nix/store/xyz-fixed.drv",["out"])],[],"x86_64-linux","/bin/sh",["-c","echo"],[("name","dependent"),("out","/nix/store/def-dependent"),("system","x86_64-linux")])"#,
            ).unwrap()
        }

        #[test]
        fn fod_hash_matches_fingerprint() {
            use sha2::{Digest, Sha256};

            let drv = fod_drv();
            assert!(drv.is_fixed_output());

            let mut cache = HashMap::new();
            let resolve = |_: &str| -> Option<&Derivation> { None };
            let hash =
                hash_derivation_modulo(&drv, "/nix/store/xyz-fixed.drv", &resolve, &mut cache)
                    .unwrap();

            // Expected: SHA-256("fixed:out:sha256:e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855:")
            let expected: [u8; 32] = Sha256::digest(
                b"fixed:out:sha256:e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855:",
            ).into();

            assert_eq!(hash, expected);
        }

        #[test]
        fn leaf_ia_hash_equals_aterm_hash() {
            use sha2::{Digest, Sha256};

            let drv = leaf_ia_drv();
            assert!(!drv.is_fixed_output());
            assert!(!drv.has_ca_floating_outputs());

            let mut cache = HashMap::new();
            let resolve = |_: &str| -> Option<&Derivation> { None };
            let hash =
                hash_derivation_modulo(&drv, "/nix/store/abc-leaf.drv", &resolve, &mut cache)
                    .unwrap();

            // Leaf IA with no inputDrvs: to_aterm_modulo(empty, false) == to_aterm()
            let expected: [u8; 32] = Sha256::digest(drv.to_aterm().as_bytes()).into();
            assert_eq!(hash, expected);
        }

        #[test]
        fn ia_with_fod_input_uses_modular_hash() {
            use sha2::{Digest, Sha256};

            let fod = fod_drv();
            let dep = ia_with_fod_input();

            let mut cache = HashMap::new();
            let resolve = |path: &str| -> Option<&Derivation> {
                if path == "/nix/store/xyz-fixed.drv" {
                    Some(&fod)
                } else {
                    None
                }
            };
            let hash =
                hash_derivation_modulo(&dep, "/nix/store/def-dependent.drv", &resolve, &mut cache)
                    .unwrap();

            // The FOD modular hash
            let fod_hash: [u8; 32] = Sha256::digest(
                b"fixed:out:sha256:e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855:",
            ).into();
            let fod_hex = hex::encode(fod_hash);

            // The modified ATerm should have the FOD hex hash instead of the drv path
            let mut rewrites = BTreeMap::new();
            rewrites.insert("/nix/store/xyz-fixed.drv".to_string(), fod_hex.clone());
            let modified_aterm = dep.to_aterm_modulo(&rewrites, false).unwrap();

            // Verify the modified ATerm contains the hex hash, not the drv path
            assert!(modified_aterm.contains(&fod_hex));
            assert!(!modified_aterm.contains("/nix/store/xyz-fixed.drv"));

            let expected: [u8; 32] = Sha256::digest(modified_aterm.as_bytes()).into();
            assert_eq!(hash, expected);
        }

        #[test]
        fn chained_ia_depth_2() {
            // Chain: C depends on B depends on FOD A
            let fod_a = fod_drv();
            let b = ia_with_fod_input(); // depends on FOD
            let c = Derivation::parse(
                r#"Derive([("out","/nix/store/ghi-chain","","")],[("/nix/store/def-dependent.drv",["out"])],[],"x86_64-linux","/bin/sh",["-c","echo"],[("name","chain"),("out","/nix/store/ghi-chain"),("system","x86_64-linux")])"#,
            ).unwrap();

            let mut cache = HashMap::new();
            let resolve = |path: &str| -> Option<&Derivation> {
                match path {
                    "/nix/store/xyz-fixed.drv" => Some(&fod_a),
                    "/nix/store/def-dependent.drv" => Some(&b),
                    _ => None,
                }
            };

            let hash = hash_derivation_modulo(&c, "/nix/store/ghi-chain.drv", &resolve, &mut cache)
                .unwrap();

            // Both A and B should now be cached
            assert!(cache.contains_key("/nix/store/xyz-fixed.drv"));
            assert!(cache.contains_key("/nix/store/def-dependent.drv"));
            assert!(cache.contains_key("/nix/store/ghi-chain.drv"));

            // Verify determinism: computing again gives same result
            let hash2 =
                hash_derivation_modulo(&c, "/nix/store/ghi-chain.drv", &resolve, &mut cache)
                    .unwrap();
            assert_eq!(hash, hash2);
        }

        #[test]
        fn multi_output_with_outputs_from_drv() {
            use crate::protocol::build::{BuildResult, BuildStatus};

            let drv = Derivation::parse(
                r#"Derive([("dev","/nix/store/abc-dev","",""),("lib","/nix/store/abc-lib","",""),("out","/nix/store/abc-out","","")],[],[],"x86_64-linux","/bin/sh",["-c","echo"],[("name","multi"),("out","/nix/store/abc-out"),("system","x86_64-linux")])"#,
            ).unwrap();

            let mut cache = HashMap::new();
            let resolve = |_: &str| -> Option<&Derivation> { None };
            let hash =
                hash_derivation_modulo(&drv, "/nix/store/abc.drv", &resolve, &mut cache).unwrap();
            let drv_hash_hex = hex::encode(hash);

            let result = BuildResult::success().with_outputs_from_drv(&drv, &drv_hash_hex);
            assert_eq!(result.status(), BuildStatus::Built);
            assert_eq!(result.built_outputs().len(), 3);

            // Outputs should be in derivation order with correct IDs and paths
            assert_eq!(
                result.built_outputs()[0].drv_output_id,
                format!("sha256:{drv_hash_hex}!dev")
            );
            assert_eq!(result.built_outputs()[0].out_path, "/nix/store/abc-dev");

            assert_eq!(
                result.built_outputs()[1].drv_output_id,
                format!("sha256:{drv_hash_hex}!lib")
            );
            assert_eq!(result.built_outputs()[1].out_path, "/nix/store/abc-lib");

            assert_eq!(
                result.built_outputs()[2].drv_output_id,
                format!("sha256:{drv_hash_hex}!out")
            );
            assert_eq!(result.built_outputs()[2].out_path, "/nix/store/abc-out");
        }

        #[test]
        fn ca_floating_masks_output_paths() {
            // CA floating: hash_algo set, hash empty
            let ca_drv = Derivation::parse(
                r#"Derive([("out","/nix/store/placeholder-ca","sha256","")],[],[],"x86_64-linux","/bin/sh",["-c","echo"],[("name","ca-test"),("out","/nix/store/placeholder-ca"),("system","x86_64-linux")])"#,
            ).unwrap();

            assert!(ca_drv.has_ca_floating_outputs());
            assert!(!ca_drv.is_fixed_output()); // hash is empty, so not FOD

            let mut cache = HashMap::new();
            let resolve = |_: &str| -> Option<&Derivation> { None };
            let hash =
                hash_derivation_modulo(&ca_drv, "/nix/store/ca.drv", &resolve, &mut cache).unwrap();

            // Verify the hash uses masked ATerm (empty output path)
            use sha2::{Digest, Sha256};
            let masked_aterm = ca_drv.to_aterm_modulo(&BTreeMap::new(), true).unwrap();
            assert!(masked_aterm.contains(r#"("out","","sha256","")"#));
            let expected: [u8; 32] = Sha256::digest(masked_aterm.as_bytes()).into();
            assert_eq!(hash, expected);
        }

        #[test]
        fn memoization_works() {
            let drv = leaf_ia_drv();
            let mut cache = HashMap::new();
            let resolve = |_: &str| -> Option<&Derivation> { None };

            let hash1 =
                hash_derivation_modulo(&drv, "/nix/store/abc-leaf.drv", &resolve, &mut cache)
                    .unwrap();
            assert_eq!(cache.len(), 1);
            assert!(cache.contains_key("/nix/store/abc-leaf.drv"));

            let hash2 =
                hash_derivation_modulo(&drv, "/nix/store/abc-leaf.drv", &resolve, &mut cache)
                    .unwrap();
            assert_eq!(hash1, hash2);
        }

        #[test]
        fn missing_input_returns_error() {
            let dep = ia_with_fod_input(); // depends on /nix/store/xyz-fixed.drv
            let mut cache = HashMap::new();
            let resolve = |_: &str| -> Option<&Derivation> { None }; // no inputs available

            let result = hash_derivation_modulo(&dep, "/nix/store/dep.drv", &resolve, &mut cache);
            assert!(matches!(result, Err(DerivationError::InputNotFound(_))));
        }

        #[test]
        fn cycle_detection() {
            // Create a derivation that references itself
            let cyclic = Derivation::parse(
                r#"Derive([("out","/nix/store/abc-cyclic","","")],[("/nix/store/abc-cyclic.drv",["out"])],[],"x86_64-linux","/bin/sh",["-c","echo"],[("name","cyclic"),("out","/nix/store/abc-cyclic"),("system","x86_64-linux")])"#,
            ).unwrap();

            let mut cache = HashMap::new();
            let resolve = |path: &str| -> Option<&Derivation> {
                if path == "/nix/store/abc-cyclic.drv" {
                    Some(&cyclic)
                } else {
                    None
                }
            };

            let result =
                hash_derivation_modulo(&cyclic, "/nix/store/abc-cyclic.drv", &resolve, &mut cache);
            assert!(matches!(result, Err(DerivationError::CycleDetected(_))));
        }

        #[test]
        fn to_aterm_modulo_replaces_input_keys() {
            let drv = Derivation::parse(
                r#"Derive([("out","/nix/store/abc-test","","")],[("/nix/store/aaa-input.drv",["out"]),("/nix/store/bbb-input.drv",["dev","out"])],[],"x86_64-linux","/bin/sh",[],[("name","test"),("system","x86_64-linux")])"#,
            ).unwrap();

            let mut rewrites = BTreeMap::new();
            rewrites.insert(
                "/nix/store/aaa-input.drv".to_string(),
                "aaaa".repeat(16), // 64-char hex
            );
            rewrites.insert("/nix/store/bbb-input.drv".to_string(), "bbbb".repeat(16));

            let result = drv.to_aterm_modulo(&rewrites, false).unwrap();

            // Keys should be the hex hashes, not the drv paths
            assert!(result.contains(&"aaaa".repeat(16)));
            assert!(result.contains(&"bbbb".repeat(16)));
            assert!(!result.contains("/nix/store/aaa-input.drv"));
            assert!(!result.contains("/nix/store/bbb-input.drv"));

            // Output paths should be preserved (mask_outputs=false)
            assert!(result.contains("/nix/store/abc-test"));
        }

        #[test]
        fn to_aterm_modulo_sorts_by_replacement_keys() {
            // Original keys sorted: aaa < bbb
            // Replacement keys sorted: zzzz > aaaa (reversed!)
            let drv = Derivation::parse(
                r#"Derive([("out","/nix/store/abc-test","","")],[("/nix/store/aaa.drv",["out"]),("/nix/store/bbb.drv",["out"])],[],"x86_64-linux","/bin/sh",[],[("name","test"),("system","x86_64-linux")])"#,
            ).unwrap();

            let mut rewrites = BTreeMap::new();
            rewrites.insert(
                "/nix/store/aaa.drv".to_string(),
                "z".repeat(64), // sorts AFTER b...
            );
            rewrites.insert(
                "/nix/store/bbb.drv".to_string(),
                "a".repeat(64), // sorts BEFORE z...
            );

            let result = drv.to_aterm_modulo(&rewrites, false).unwrap();

            // In the ATerm, the "a" key should appear before the "z" key
            let a_pos = result.find(&"a".repeat(64)).unwrap();
            let z_pos = result.find(&"z".repeat(64)).unwrap();
            assert!(
                a_pos < z_pos,
                "replacement keys must be sorted in the ATerm"
            );
        }

        #[test]
        fn fod_recursive_hash() {
            use sha2::{Digest, Sha256};

            // Recursive FOD: hash_algo = "r:sha256"
            let drv = Derivation::parse(
                r#"Derive([("out","/nix/store/xyz-rec","r:sha256","e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855")],[],[],"x86_64-linux","/bin/sh",["-c","echo"],[("name","rec"),("out","/nix/store/xyz-rec"),("outputHash","e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"),("outputHashAlgo","r:sha256"),("system","x86_64-linux")])"#,
            ).unwrap();

            assert!(drv.is_fixed_output());

            let mut cache = HashMap::new();
            let resolve = |_: &str| -> Option<&Derivation> { None };
            let hash = hash_derivation_modulo(&drv, "/nix/store/xyz-rec.drv", &resolve, &mut cache)
                .unwrap();

            // Expected: SHA-256("fixed:out:r:sha256:e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855:")
            let expected: [u8; 32] = Sha256::digest(
                b"fixed:out:r:sha256:e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855:",
            ).into();

            assert_eq!(hash, expected);
        }

        #[test]
        fn diamond_dag_memoization() {
            // Diamond: D depends on B and C, both depend on FOD A
            let a = fod_drv();
            let b = ia_with_fod_input(); // depends on /nix/store/xyz-fixed.drv
            let c = Derivation::parse(
                r#"Derive([("out","/nix/store/ccc-other","","")],[("/nix/store/xyz-fixed.drv",["out"])],[],"x86_64-linux","/bin/sh",["-c","echo"],[("name","other"),("out","/nix/store/ccc-other"),("system","x86_64-linux")])"#,
            ).unwrap();
            let d = Derivation::parse(
                r#"Derive([("out","/nix/store/ddd-diamond","","")],[("/nix/store/ccc-other.drv",["out"]),("/nix/store/def-dependent.drv",["out"])],[],"x86_64-linux","/bin/sh",["-c","echo"],[("name","diamond"),("out","/nix/store/ddd-diamond"),("system","x86_64-linux")])"#,
            ).unwrap();

            let mut cache = HashMap::new();
            let resolve = |path: &str| -> Option<&Derivation> {
                match path {
                    "/nix/store/xyz-fixed.drv" => Some(&a),
                    "/nix/store/def-dependent.drv" => Some(&b),
                    "/nix/store/ccc-other.drv" => Some(&c),
                    _ => None,
                }
            };

            // Should not produce a false CycleDetected for the shared FOD A
            let hash =
                hash_derivation_modulo(&d, "/nix/store/ddd-diamond.drv", &resolve, &mut cache)
                    .unwrap();

            // All 4 should be cached
            assert_eq!(cache.len(), 4);
            assert!(cache.contains_key("/nix/store/xyz-fixed.drv"));

            // Determinism check
            let hash2 =
                hash_derivation_modulo(&d, "/nix/store/ddd-diamond.drv", &resolve, &mut cache)
                    .unwrap();
            assert_eq!(hash, hash2);
        }

        #[test]
        fn indirect_cycle_detection() {
            // A -> B -> A (indirect cycle)
            let a = Derivation::parse(
                r#"Derive([("out","/nix/store/aaa-cycle","","")],[("/nix/store/bbb-cycle.drv",["out"])],[],"x86_64-linux","/bin/sh",["-c","echo"],[("name","a"),("system","x86_64-linux")])"#,
            ).unwrap();
            let b = Derivation::parse(
                r#"Derive([("out","/nix/store/bbb-cycle","","")],[("/nix/store/aaa-cycle.drv",["out"])],[],"x86_64-linux","/bin/sh",["-c","echo"],[("name","b"),("system","x86_64-linux")])"#,
            ).unwrap();

            let mut cache = HashMap::new();
            let resolve = |path: &str| -> Option<&Derivation> {
                match path {
                    "/nix/store/aaa-cycle.drv" => Some(&a),
                    "/nix/store/bbb-cycle.drv" => Some(&b),
                    _ => None,
                }
            };

            let result =
                hash_derivation_modulo(&a, "/nix/store/aaa-cycle.drv", &resolve, &mut cache);
            assert!(matches!(result, Err(DerivationError::CycleDetected(_))));
        }

        #[test]
        fn with_outputs_from_drv_produces_correct_ids() {
            use crate::protocol::build::{BuildResult, BuildStatus};

            let drv = fod_drv(); // FOD with known hash
            let mut cache = HashMap::new();
            let resolve = |_: &str| -> Option<&Derivation> { None };
            let hash =
                hash_derivation_modulo(&drv, "/nix/store/xyz-fixed.drv", &resolve, &mut cache)
                    .unwrap();
            let drv_hash_hex = hex::encode(hash);

            let result = BuildResult::success().with_outputs_from_drv(&drv, &drv_hash_hex);

            assert_eq!(result.status(), BuildStatus::Built);
            assert_eq!(result.built_outputs().len(), 1);
            assert_eq!(
                result.built_outputs()[0].drv_output_id,
                format!("sha256:{drv_hash_hex}!out")
            );
            assert_eq!(result.built_outputs()[0].out_path, "/nix/store/xyz-fixed");
        }

        #[test]
        fn to_aterm_modulo_no_rewrites_matches_to_aterm() {
            let drv = leaf_ia_drv();
            let modulo = drv.to_aterm_modulo(&BTreeMap::new(), false).unwrap();
            assert_eq!(modulo, drv.to_aterm());
        }

        #[test]
        fn to_aterm_modulo_missing_key_errors() {
            let drv = Derivation::parse(
                r#"Derive([("out","/nix/store/abc-test","","")],[("/nix/store/missing.drv",["out"])],[],"x86_64-linux","/bin/sh",[],[("name","test"),("system","x86_64-linux")])"#,
            ).unwrap();

            let result = drv.to_aterm_modulo(&BTreeMap::new(), false);
            assert!(matches!(result, Err(DerivationError::InputNotFound(_))));
        }
    }

    mod proptests {
        use super::*;
        use proptest::prelude::*;

        fn arb_output() -> impl Strategy<Value = DerivationOutput> {
            (
                "[a-z]{1,8}",                                                 // name
                "/nix/store/[a-z0-9]{32}-[a-z]{1,10}",                        // path
                prop_oneof![Just(String::new()), Just("sha256".to_string())], // hash_algo
            )
                .prop_map(|(name, path, hash_algo)| {
                    let hash = if hash_algo.is_empty() {
                        String::new()
                    } else {
                        "0".repeat(64) // 64-char hex digest
                    };
                    DerivationOutput::new(name, path, hash_algo, hash).unwrap()
                })
        }

        fn arb_derivation() -> impl Strategy<Value = Derivation> {
            (
                proptest::collection::vec(arb_output(), 1..4),
                proptest::collection::vec(
                    (
                        "/nix/store/[a-z0-9]{32}-[a-z]{1,8}\\.drv",
                        proptest::collection::btree_set("[a-z]{1,5}", 1..3),
                    ),
                    0..3,
                ),
                proptest::collection::btree_set("/nix/store/[a-z0-9]{32}-[a-z]{1,8}", 0..3),
                "(x86_64|aarch64)-linux",
                "/nix/store/[a-z0-9]{32}-bash/bin/bash",
                proptest::collection::vec("[a-zA-Z0-9 ./-]{0,20}", 0..4),
                proptest::collection::btree_map("[a-zA-Z_]{1,10}", "[a-zA-Z0-9 =/_.-]{0,30}", 0..5),
            )
                .prop_map(
                    |(outputs, input_drvs_vec, input_srcs, platform, builder, args, env)| {
                        let input_drvs: BTreeMap<String, BTreeSet<String>> =
                            input_drvs_vec.into_iter().collect();
                        Derivation {
                            outputs,
                            input_drvs,
                            input_srcs,
                            platform,
                            builder,
                            args,
                            env,
                        }
                    },
                )
        }

        proptest! {
            #![proptest_config(ProptestConfig::with_cases(4096))]
            #[test]
            fn aterm_roundtrip(drv in arb_derivation()) {
                let serialized = drv.to_aterm();
                let reparsed = Derivation::parse(&serialized).unwrap();
                prop_assert_eq!(drv, reparsed);
            }
        }
    }
}
