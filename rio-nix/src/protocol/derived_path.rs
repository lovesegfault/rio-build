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
// r[impl gw.wire.derived-path]

use crate::store_path::{StorePath, StorePathError};

/// Errors from parsing a `DerivedPath` string.
#[derive(Debug, thiserror::Error)]
pub enum DerivedPathError {
    #[error(transparent)]
    StorePath(#[from] StorePathError),

    #[error("output name must not be empty")]
    EmptyOutputName,

    #[error("duplicate output name")]
    DuplicateOutputName,
}

/// Which outputs to build from a derivation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OutputSpec {
    /// Build all outputs (`!*`).
    All,
    /// Build specific named outputs (`!out,dev`). Validated non-empty
    /// and each name non-empty via [`OutputSpec::names`].
    Names(Vec<String>),
}

impl OutputSpec {
    /// Create a `Names` variant after validating that the list is non-empty
    /// and every name is non-empty.
    pub fn names(names: Vec<String>) -> Result<Self, DerivedPathError> {
        if names.is_empty() || names.iter().any(|n| n.is_empty()) {
            return Err(DerivedPathError::EmptyOutputName);
        }
        let unique: std::collections::HashSet<&str> = names.iter().map(|n| n.as_str()).collect();
        if unique.len() != names.len() {
            return Err(DerivedPathError::DuplicateOutputName);
        }
        Ok(OutputSpec::Names(names))
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
                    for (i, name) in names.iter().enumerate() {
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
    fn parse_opaque() -> anyhow::Result<()> {
        let path_str = make_path("hello-2.12.1");
        let dp = DerivedPath::parse(&path_str)?;
        match dp {
            DerivedPath::Opaque(p) => {
                assert_eq!(p.name(), "hello-2.12.1");
            }
            DerivedPath::Built { .. } => panic!("expected Opaque"),
        }
        Ok(())
    }

    #[test]
    fn parse_built_all_outputs() -> anyhow::Result<()> {
        let path_str = format!("{}!*", make_path("hello-2.12.1.drv"));
        let dp = DerivedPath::parse(&path_str)?;
        match dp {
            DerivedPath::Built { drv, outputs } => {
                assert!(drv.is_derivation());
                assert_eq!(drv.name(), "hello-2.12.1.drv");
                assert_eq!(outputs, OutputSpec::All);
            }
            DerivedPath::Opaque(_) => panic!("expected Built"),
        }
        Ok(())
    }

    #[test]
    fn parse_built_explicit_outputs() -> anyhow::Result<()> {
        let path_str = format!("{}!out,dev", make_path("hello-2.12.1.drv"));
        let dp = DerivedPath::parse(&path_str)?;
        match dp {
            DerivedPath::Built { drv, outputs } => {
                assert!(drv.is_derivation());
                match &outputs {
                    OutputSpec::Names(names) => {
                        assert_eq!(names, &["out".to_string(), "dev".to_string()]);
                    }
                    OutputSpec::All => panic!("expected Names"),
                }
            }
            DerivedPath::Opaque(_) => panic!("expected Built"),
        }
        Ok(())
    }

    #[test]
    fn parse_built_single_output() -> anyhow::Result<()> {
        let path_str = format!("{}!out", make_path("hello-2.12.1.drv"));
        let dp = DerivedPath::parse(&path_str)?;
        match dp {
            DerivedPath::Built { outputs, .. } => match &outputs {
                OutputSpec::Names(names) => {
                    assert_eq!(names, &["out".to_string()]);
                }
                OutputSpec::All => panic!("expected Names"),
            },
            DerivedPath::Opaque(_) => panic!("expected Built"),
        }
        Ok(())
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
    fn store_path_extracts_correctly() -> anyhow::Result<()> {
        let opaque_str = make_path("hello-2.12.1");
        let opaque = DerivedPath::parse(&opaque_str)?;
        assert_eq!(opaque.store_path().name(), "hello-2.12.1");

        let built_str = format!("{}!*", make_path("hello-2.12.1.drv"));
        let built = DerivedPath::parse(&built_str)?;
        assert_eq!(built.store_path().name(), "hello-2.12.1.drv");
        Ok(())
    }

    #[test]
    fn parse_multiple_bang_separators() -> anyhow::Result<()> {
        // split_once('!') means only the first '!' is the separator
        let path_str = format!("{}!out!extra", make_path("hello.drv"));
        let dp = DerivedPath::parse(&path_str)?;
        match dp {
            DerivedPath::Built { outputs, .. } => {
                // "out!extra" is treated as a single output name (with ! in it)
                match &outputs {
                    OutputSpec::Names(names) => assert_eq!(names, &["out!extra"]),
                    OutputSpec::All => panic!("expected Names"),
                }
            }
            DerivedPath::Opaque(_) => panic!("expected Built"),
        }
        Ok(())
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
            OutputSpec::names(vec![String::new()]),
            Err(DerivedPathError::EmptyOutputName)
        ));
    }

    #[test]
    fn output_spec_names_rejects_duplicates() {
        assert!(matches!(
            OutputSpec::names(vec!["out".to_string(), "out".to_string()]),
            Err(DerivedPathError::DuplicateOutputName)
        ));
    }

    #[test]
    fn output_spec_names_accepts_valid() -> anyhow::Result<()> {
        let spec = OutputSpec::names(vec!["out".to_string(), "dev".to_string()])?;
        match &spec {
            OutputSpec::Names(names) => {
                assert_eq!(names, &["out".to_string(), "dev".to_string()]);
            }
            OutputSpec::All => panic!("expected Names"),
        }
        Ok(())
    }

    #[test]
    fn display_roundtrip_opaque() -> anyhow::Result<()> {
        let path_str = make_path("hello-2.12.1");
        let dp = DerivedPath::parse(&path_str)?;
        let roundtripped = DerivedPath::parse(&dp.to_string())?;
        assert_eq!(dp, roundtripped);
        Ok(())
    }

    #[test]
    fn display_roundtrip_built_all() -> anyhow::Result<()> {
        let path_str = format!("{}!*", make_path("hello.drv"));
        let dp = DerivedPath::parse(&path_str)?;
        let roundtripped = DerivedPath::parse(&dp.to_string())?;
        assert_eq!(dp, roundtripped);
        Ok(())
    }

    #[test]
    fn display_roundtrip_built_named() -> anyhow::Result<()> {
        let path_str = format!("{}!out,dev,lib", make_path("hello.drv"));
        let dp = DerivedPath::parse(&path_str)?;
        let roundtripped = DerivedPath::parse(&dp.to_string())?;
        assert_eq!(dp, roundtripped);
        Ok(())
    }

    mod proptests {
        use super::*;
        use proptest::prelude::*;

        fn arb_derived_path() -> impl Strategy<Value = DerivedPath> {
            // Generate a valid store path name component
            let name_re = "[a-zA-Z][a-zA-Z0-9._+-]{0,20}";

            let opaque = name_re.prop_map(|n| {
                let path_str = format!("/nix/store/{VALID_HASH}-{n}");
                DerivedPath::Opaque(StorePath::parse(&path_str).expect("generated to be valid"))
            });

            let built_all = name_re.prop_map(|n| {
                let path_str = format!("/nix/store/{VALID_HASH}-{n}.drv");
                DerivedPath::Built {
                    drv: StorePath::parse(&path_str).expect("generated to be valid"),
                    outputs: OutputSpec::All,
                }
            });

            let built_named = (name_re, proptest::collection::vec("[a-z]{1,8}", 1..4))
                .prop_filter_map("output names must be unique", |(n, output_names)| {
                    let path_str = format!("/nix/store/{VALID_HASH}-{n}.drv");
                    OutputSpec::names(output_names)
                        .ok()
                        .map(|outputs| DerivedPath::Built {
                            drv: StorePath::parse(&path_str).expect("generated to be valid"),
                            outputs,
                        })
                });

            prop_oneof![opaque, built_all, built_named]
        }

        proptest! {
            #![proptest_config(ProptestConfig::with_cases(4096))]
            #[test]
            fn derived_path_roundtrip(dp in arb_derived_path()) {
                let s = dp.to_string();
                let roundtripped = DerivedPath::parse(&s)?;
                prop_assert_eq!(dp, roundtripped);
            }
        }
    }
}
