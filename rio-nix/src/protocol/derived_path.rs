//! DerivedPath parsing for the Nix worker protocol.
//!
//! A `DerivedPath` is sent by Nix clients in `wopBuildPaths`, `wopBuildPathsWithResults`,
//! and `wopQueryMissing` to specify what to build. It has three forms:
//!
//! - **Opaque:** plain store path — `/nix/store/abc...-foo`
//! - **Built (explicit outputs):** `/nix/store/abc...-foo.drv!out,dev`
//! - **Built (all outputs):** `/nix/store/abc...-foo.drv!*`
//!
//! See `gateway.md` for the full wire format specification.

use crate::store_path::{StorePath, StorePathError};

/// Errors from parsing a `DerivedPath` string.
#[derive(Debug, thiserror::Error)]
pub enum DerivedPathError {
    #[error(transparent)]
    StorePath(#[from] StorePathError),

    #[error("output name must not be empty")]
    EmptyOutputName,
}

/// A validated, non-empty collection of output names, each guaranteed non-empty.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OutputNames {
    names: Vec<String>,
}

impl OutputNames {
    /// The output names.
    pub fn names(&self) -> &[String] {
        &self.names
    }
}

/// Which outputs to build from a derivation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OutputSpec {
    /// Build all outputs (`!*`).
    All,
    /// Build specific named outputs (`!out,dev`).
    Names(OutputNames),
}

impl OutputSpec {
    /// Create a `Names` variant after validating that the list is non-empty
    /// and every name is non-empty.
    pub fn names(names: Vec<String>) -> Result<Self, DerivedPathError> {
        if names.is_empty() || names.iter().any(|n| n.is_empty()) {
            return Err(DerivedPathError::EmptyOutputName);
        }
        Ok(OutputSpec::Names(OutputNames { names }))
    }
}

/// A path specification sent by Nix clients indicating what to build or query.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DerivedPath {
    /// A plain store path (no output specifier).
    Opaque(StorePath),
    /// A derivation with an output specifier.
    Built { drv: StorePath, outputs: OutputSpec },
}

impl DerivedPath {
    /// Parse a DerivedPath string.
    ///
    /// Splits on `!` to separate the store path from the output spec.
    /// If no `!` is present, the entire string is treated as an opaque store path.
    pub fn parse(s: &str) -> Result<Self, DerivedPathError> {
        if let Some((drv_part, output_part)) = s.split_once('!') {
            let drv = StorePath::parse(drv_part)?;
            let outputs = if output_part == "*" {
                OutputSpec::All
            } else {
                let names: Vec<String> = output_part.split(',').map(String::from).collect();
                OutputSpec::names(names)?
            };
            Ok(DerivedPath::Built { drv, outputs })
        } else {
            Ok(DerivedPath::Opaque(StorePath::parse(s)?))
        }
    }

    /// Extract the base store path.
    ///
    /// For `Opaque`, returns the path itself.
    /// For `Built`, returns the derivation path.
    pub fn store_path(&self) -> &StorePath {
        match self {
            DerivedPath::Opaque(p) => p,
            DerivedPath::Built { drv, .. } => drv,
        }
    }
}

impl std::fmt::Display for DerivedPath {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            DerivedPath::Opaque(path) => write!(f, "{path}"),
            DerivedPath::Built { drv, outputs } => match outputs {
                OutputSpec::All => write!(f, "{drv}!*"),
                OutputSpec::Names(names) => {
                    write!(f, "{drv}!")?;
                    for (i, name) in names.names().iter().enumerate() {
                        if i > 0 {
                            write!(f, ",")?;
                        }
                        write!(f, "{name}")?;
                    }
                    Ok(())
                }
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const VALID_HASH: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";

    fn make_path(name: &str) -> String {
        format!("/nix/store/{VALID_HASH}-{name}")
    }

    #[test]
    fn parse_opaque() {
        let path_str = make_path("hello-2.12.1");
        let dp = DerivedPath::parse(&path_str).unwrap();
        match dp {
            DerivedPath::Opaque(p) => {
                assert_eq!(p.name(), "hello-2.12.1");
            }
            DerivedPath::Built { .. } => panic!("expected Opaque"),
        }
    }

    #[test]
    fn parse_built_all_outputs() {
        let path_str = format!("{}!*", make_path("hello-2.12.1.drv"));
        let dp = DerivedPath::parse(&path_str).unwrap();
        match dp {
            DerivedPath::Built { drv, outputs } => {
                assert!(drv.is_derivation());
                assert_eq!(drv.name(), "hello-2.12.1.drv");
                assert_eq!(outputs, OutputSpec::All);
            }
            DerivedPath::Opaque(_) => panic!("expected Built"),
        }
    }

    #[test]
    fn parse_built_explicit_outputs() {
        let path_str = format!("{}!out,dev", make_path("hello-2.12.1.drv"));
        let dp = DerivedPath::parse(&path_str).unwrap();
        match dp {
            DerivedPath::Built { drv, outputs } => {
                assert!(drv.is_derivation());
                match &outputs {
                    OutputSpec::Names(output_names) => {
                        assert_eq!(
                            output_names.names(),
                            &["out".to_string(), "dev".to_string()]
                        );
                    }
                    _ => panic!("expected Names"),
                }
            }
            DerivedPath::Opaque(_) => panic!("expected Built"),
        }
    }

    #[test]
    fn parse_built_single_output() {
        let path_str = format!("{}!out", make_path("hello-2.12.1.drv"));
        let dp = DerivedPath::parse(&path_str).unwrap();
        match dp {
            DerivedPath::Built { outputs, .. } => match &outputs {
                OutputSpec::Names(output_names) => {
                    assert_eq!(output_names.names(), &["out".to_string()]);
                }
                _ => panic!("expected Names"),
            },
            DerivedPath::Opaque(_) => panic!("expected Built"),
        }
    }

    #[test]
    fn parse_invalid_base_path() {
        assert!(DerivedPath::parse("not-a-path!*").is_err());
        assert!(DerivedPath::parse("not-a-path").is_err());
    }

    #[test]
    fn parse_rejects_empty_output_name() {
        // Trailing comma: "out,"
        let path_str = format!("{}!out,", make_path("hello-2.12.1.drv"));
        assert!(matches!(
            DerivedPath::parse(&path_str),
            Err(DerivedPathError::EmptyOutputName)
        ));

        // Leading comma: ",out"
        let path_str = format!("{}!,out", make_path("hello-2.12.1.drv"));
        assert!(matches!(
            DerivedPath::parse(&path_str),
            Err(DerivedPathError::EmptyOutputName)
        ));

        // Adjacent commas: "out,,dev"
        let path_str = format!("{}!out,,dev", make_path("hello-2.12.1.drv"));
        assert!(matches!(
            DerivedPath::parse(&path_str),
            Err(DerivedPathError::EmptyOutputName)
        ));

        // Bare bang with no output names
        let path_str = format!("{}!", make_path("hello-2.12.1.drv"));
        assert!(matches!(
            DerivedPath::parse(&path_str),
            Err(DerivedPathError::EmptyOutputName)
        ));
    }

    #[test]
    fn store_path_extracts_correctly() {
        let opaque_str = make_path("hello-2.12.1");
        let opaque = DerivedPath::parse(&opaque_str).unwrap();
        assert_eq!(opaque.store_path().name(), "hello-2.12.1");

        let built_str = format!("{}!*", make_path("hello-2.12.1.drv"));
        let built = DerivedPath::parse(&built_str).unwrap();
        assert_eq!(built.store_path().name(), "hello-2.12.1.drv");
    }

    #[test]
    fn parse_multiple_bang_separators() {
        // split_once('!') means only the first '!' is the separator
        let path_str = format!("{}!out!extra", make_path("hello.drv"));
        let dp = DerivedPath::parse(&path_str).unwrap();
        match dp {
            DerivedPath::Built { outputs, .. } => {
                // "out!extra" is treated as a single output name (with ! in it)
                match &outputs {
                    OutputSpec::Names(names) => assert_eq!(names.names(), &["out!extra"]),
                    _ => panic!("expected Names"),
                }
            }
            DerivedPath::Opaque(_) => panic!("expected Built"),
        }
    }

    #[test]
    fn output_spec_names_rejects_empty_vec() {
        assert!(matches!(
            OutputSpec::names(vec![]),
            Err(DerivedPathError::EmptyOutputName)
        ));
    }

    #[test]
    fn output_spec_names_rejects_empty_name() {
        assert!(matches!(
            OutputSpec::names(vec!["".to_string()]),
            Err(DerivedPathError::EmptyOutputName)
        ));
    }

    #[test]
    fn output_spec_names_accepts_valid() {
        let spec = OutputSpec::names(vec!["out".to_string(), "dev".to_string()]).unwrap();
        match &spec {
            OutputSpec::Names(output_names) => {
                assert_eq!(
                    output_names.names(),
                    &["out".to_string(), "dev".to_string()]
                );
            }
            _ => panic!("expected Names"),
        }
    }
}
