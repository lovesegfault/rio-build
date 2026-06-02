//! Typed derivation outputs — the parse-boundary chokepoint.
//!
//! Mirrors CppNix's `parseDerivationOutput` (derivations.cc:306-354):
//! the (path, hashAlgo, hash) field triple is classified ONCE, at
//! construction, into the four legal shapes — input-addressed,
//! deferred, fixed-output, floating-CA — and every illegal shape is
//! unrepresentable from that point on. Both untrusted parsers (the
//! ATerm parser and the wire `read_basic_derivation`) and every public
//! constructor route through [`DerivationOutput::new`], so no consumer
//! downstream of a parse can observe a malformed declared output path,
//! a floating output with a declared path (derivations.cc:339-340), or
//! a fixed output without one (`validatePath`, derivations.cc:271-275).
//!
//! Three deliberate divergences from the oracle, all fail-closed:
//!
//! 1. **Path validation is full `StorePath::parse`**, not the oracle's
//!    leading-`/` check — rio's trust model forwards declared output
//!    paths to workers and joins them against the overlay upper store,
//!    so "parses as a store path" is the load-bearing property.
//! 2. **`hash` without `hashAlgo` is rejected** ([`DerivationError::HashWithoutAlgo`]).
//!    The oracle's parser silently drops the hash on the unparse side
//!    (derivations.cc:346 reads the pair only under a non-empty algo),
//!    which would break rio's byte-faithful round-trip guarantee. No
//!    honest producer emits this shape.
//! 3. **`impure` outputs are not special-cased** — an `"impure"` hash
//!    sentinel classifies as Fixed with an undecodable hash and is
//!    rejected by the downstream decode gates (default experimental-
//!    feature posture: `Xp::ImpureDerivations` is off).
//!
//! Junk hash *values* and junk algo *names* remain representable (raw
//! `String` fields) — the gateway's realized-offender exemption flow
//! requires carrying undecodable hashes through to its probe.

use crate::store_path::StorePath;

use super::DerivationError;

/// A single derivation output.
///
/// The classified shape is private ([`OutputRepr`]); consumers either
/// use the legacy string accessors (`path()` returns `""` for shapes
/// without one — byte-compatible with the pre-typed struct) or match
/// on [`DerivationOutput::kind`] for shape-dispatched logic.
#[derive(Debug, Clone)]
pub struct DerivationOutput {
    name: String,
    repr: OutputRepr,
}

/// The four legal output shapes (CppNix `DerivationOutput` variants,
/// minus `Impure` — see the module doc).
#[derive(Debug, Clone)]
enum OutputRepr {
    /// Concrete input-addressed: path declared, no hash fields.
    InputAddressed { path: StorePath },
    /// Deferred input-addressed: all three fields empty; the path is
    /// computed once floating inputs resolve.
    Deferred,
    /// Fixed-output: declared path + declared hash algo and digest.
    Fixed {
        path: StorePath,
        hash_algo: String,
        hash: String,
    },
    /// Floating content-addressed: algo declared, path and digest
    /// derived from content after the build.
    Floating { hash_algo: String },
}

/// Borrowed view of an output's classified shape, for dispatch.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OutputKind<'a> {
    /// Concrete input-addressed output at `path`.
    InputAddressed(&'a StorePath),
    /// Deferred input-addressed output (path not yet known).
    Deferred,
    /// Fixed-output with declared path and hash.
    Fixed {
        /// Declared store path.
        path: &'a StorePath,
        /// Declared hash algorithm (raw, possibly junk — decode gates
        /// own rejection).
        hash_algo: &'a str,
        /// Declared digest (raw, possibly junk).
        hash: &'a str,
    },
    /// Floating content-addressed output.
    Floating {
        /// Declared hash algorithm (raw).
        hash_algo: &'a str,
    },
}

impl DerivationOutput {
    /// Create a new derivation output, classifying the field triple
    /// into one of the four legal shapes.
    ///
    /// Rejections (see the module doc for oracle citations):
    /// - empty `name`
    /// - non-empty `path` that does not parse as a store path
    /// - `hash_algo` + `hash` with an empty path (fixed output must
    ///   declare its path)
    /// - `hash_algo` without `hash` but WITH a path (floating outputs
    ///   must not declare paths)
    /// - `hash` without `hash_algo`
    // r[impl nix.drv.output-typed]
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
        let path = path.into();
        let hash_algo = hash_algo.into();
        let hash = hash.into();

        if !hash.is_empty() && hash_algo.is_empty() {
            return Err(DerivationError::HashWithoutAlgo(name));
        }

        let repr = if !hash_algo.is_empty() {
            if !hash.is_empty() {
                // Oracle: CAFixed — validatePath(pathS) (an empty path
                // is "bad path '' in derivation").
                if path.is_empty() {
                    return Err(DerivationError::FixedOutputNoPath(name));
                }
                let path = StorePath::parse(&path)?;
                OutputRepr::Fixed {
                    path,
                    hash_algo,
                    hash,
                }
            } else {
                // Oracle: CAFloating — "should not specify output path".
                if !path.is_empty() {
                    return Err(DerivationError::FloatingCaDeclaredPath(name));
                }
                OutputRepr::Floating { hash_algo }
            }
        } else if path.is_empty() {
            OutputRepr::Deferred
        } else {
            OutputRepr::InputAddressed {
                path: StorePath::parse(&path)?,
            }
        };

        Ok(DerivationOutput { name, repr })
    }

    /// The output name.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// The output store path, or `""` for shapes without one
    /// (deferred and floating-CA).
    ///
    /// Byte-compatible with the pre-typed accessor: for shapes WITH a
    /// path this is the verbatim declared string (`StorePath::parse`
    /// is strict-canonical, so parse → `as_str()` is the identity).
    pub fn path(&self) -> &str {
        match &self.repr {
            OutputRepr::InputAddressed { path } | OutputRepr::Fixed { path, .. } => path.as_str(),
            OutputRepr::Deferred | OutputRepr::Floating { .. } => "",
        }
    }

    /// Hash algorithm (empty for input-addressed and deferred).
    pub fn hash_algo(&self) -> &str {
        match &self.repr {
            OutputRepr::Fixed { hash_algo, .. } | OutputRepr::Floating { hash_algo } => hash_algo,
            OutputRepr::InputAddressed { .. } | OutputRepr::Deferred => "",
        }
    }

    /// Expected hash (set only for fixed outputs).
    pub fn hash(&self) -> &str {
        match &self.repr {
            OutputRepr::Fixed { hash, .. } => hash,
            _ => "",
        }
    }

    /// Whether this output has a `hash_algo` set.
    ///
    /// Returns true for *any* content-addressed output — both
    /// fixed-output and floating-CA. To distinguish FOD from
    /// floating-CA, match on [`DerivationOutput::kind`].
    ///
    /// See `DerivationLike::is_fixed_output` for the strict FOD
    /// predicate (single `out` output of Fixed shape).
    pub fn has_hash_algo(&self) -> bool {
        matches!(
            self.repr,
            OutputRepr::Fixed { .. } | OutputRepr::Floating { .. }
        )
    }

    /// The classified shape, for dispatch.
    pub fn kind(&self) -> OutputKind<'_> {
        match &self.repr {
            OutputRepr::InputAddressed { path } => OutputKind::InputAddressed(path),
            OutputRepr::Deferred => OutputKind::Deferred,
            OutputRepr::Fixed {
                path,
                hash_algo,
                hash,
            } => OutputKind::Fixed {
                path,
                hash_algo,
                hash,
            },
            OutputRepr::Floating { hash_algo } => OutputKind::Floating { hash_algo },
        }
    }

    /// The typed store path, for shapes that declare one.
    pub fn store_path(&self) -> Option<&StorePath> {
        match &self.repr {
            OutputRepr::InputAddressed { path } | OutputRepr::Fixed { path, .. } => Some(path),
            OutputRepr::Deferred | OutputRepr::Floating { .. } => None,
        }
    }

    /// Fill a deferred output with its computed input-addressed path.
    ///
    /// One of exactly two non-classifying construction routes (the
    /// other is the `cfg(test)` escape hatch, if ever added): used
    /// only by `Derivation::fill_deferred_outputs`, which derives the
    /// path itself — the value never originates from an untrusted
    /// declaration. No-op error if the output is not deferred.
    pub(crate) fn fill_deferred(&mut self, path: StorePath) {
        debug_assert!(
            matches!(self.repr, OutputRepr::Deferred),
            "fill_deferred on a non-deferred output"
        );
        self.repr = OutputRepr::InputAddressed { path };
    }
}

/// Extensional equality over the legacy field view — identical to the
/// pre-typed derive (two outputs are equal iff their (name, path,
/// hash_algo, hash) string quadruples are equal).
impl PartialEq for DerivationOutput {
    fn eq(&self, other: &Self) -> bool {
        self.name == other.name
            && self.path() == other.path()
            && self.hash_algo() == other.hash_algo()
            && self.hash() == other.hash()
    }
}

impl Eq for DerivationOutput {}

#[cfg(test)]
mod tests {
    use super::*;

    const P: &str = "/nix/store/n2v52szmyja512fxmaax8lixl4dxh4jb-out";
    const H64: &str = "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";

    /// Constructor rejection matrix: every banned shape, with the
    /// oracle's wording where the oracle has one.
    // r[verify nix.drv.output-typed]
    #[test]
    fn rejection_matrix() {
        // empty name
        assert!(matches!(
            DerivationOutput::new("", P, "", ""),
            Err(DerivationError::EmptyOutputName(0))
        ));
        // malformed declared path (short hash)
        assert!(matches!(
            DerivationOutput::new("out", "/nix/store/zzz-evil", "", ""),
            Err(DerivationError::InvalidOutputPath(_))
        ));
        // malformed declared path (bad alphabet: 'e' not in nixbase32)
        assert!(matches!(
            DerivationOutput::new(
                "out",
                "/nix/store/eeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee-x",
                "",
                ""
            ),
            Err(DerivationError::InvalidOutputPath(_))
        ));
        // malformed declared path (not a store path at all)
        assert!(matches!(
            DerivationOutput::new("out", "/build/exfil", "", ""),
            Err(DerivationError::InvalidOutputPath(_))
        ));
        // floating-CA with declared path (oracle derivations.cc:339-340)
        let err = DerivationOutput::new("out", P, "sha256", "").unwrap_err();
        assert!(matches!(err, DerivationError::FloatingCaDeclaredPath(_)));
        assert!(
            err.to_string()
                .contains("content-addressing derivation output should not specify output path"),
            "oracle wording: {err}"
        );
        // fixed output without a path (oracle validatePath analog)
        let err = DerivationOutput::new("out", "", "sha256", H64).unwrap_err();
        assert!(matches!(err, DerivationError::FixedOutputNoPath(_)));
        assert!(
            err.to_string().contains("bad path ''"),
            "oracle wording: {err}"
        );
        // hash without algo (deliberate fail-closed divergence)
        assert!(matches!(
            DerivationOutput::new("out", P, "", H64),
            Err(DerivationError::HashWithoutAlgo(_))
        ));
        assert!(matches!(
            DerivationOutput::new("out", "", "", H64),
            Err(DerivationError::HashWithoutAlgo(_))
        ));
        // fixed output with malformed path still hits path validation
        assert!(matches!(
            DerivationOutput::new("out", "/nix/store/zzz-evil", "sha256", H64),
            Err(DerivationError::InvalidOutputPath(_))
        ));
    }

    /// All four legal shapes construct, classify, and expose the
    /// byte-identical legacy accessor view.
    // r[verify nix.drv.output-typed]
    #[test]
    fn legal_shapes_classify_and_roundtrip_accessors() {
        let ia = DerivationOutput::new("out", P, "", "").unwrap();
        assert!(matches!(ia.kind(), OutputKind::InputAddressed(p) if p.as_str() == P));
        assert_eq!((ia.path(), ia.hash_algo(), ia.hash()), (P, "", ""));
        assert_eq!(ia.store_path().unwrap().as_str(), P);
        assert!(!ia.has_hash_algo());

        let deferred = DerivationOutput::new("out", "", "", "").unwrap();
        assert!(matches!(deferred.kind(), OutputKind::Deferred));
        assert_eq!(
            (deferred.path(), deferred.hash_algo(), deferred.hash()),
            ("", "", "")
        );
        assert!(deferred.store_path().is_none());

        let fixed = DerivationOutput::new("out", P, "sha256", H64).unwrap();
        assert!(matches!(
            fixed.kind(),
            OutputKind::Fixed { path, hash_algo: "sha256", hash } if path.as_str() == P && hash == H64
        ));
        assert_eq!(
            (fixed.path(), fixed.hash_algo(), fixed.hash()),
            (P, "sha256", H64)
        );
        assert!(fixed.has_hash_algo());

        // "r:sha256" method-prefixed algo stays raw in the typed view.
        let floating = DerivationOutput::new("out", "", "r:sha256", "").unwrap();
        assert!(matches!(
            floating.kind(),
            OutputKind::Floating {
                hash_algo: "r:sha256"
            }
        ));
        assert_eq!(
            (floating.path(), floating.hash_algo(), floating.hash()),
            ("", "r:sha256", "")
        );
        assert!(floating.has_hash_algo());
        assert!(floating.store_path().is_none());
    }

    /// The `"impure"` hash sentinel is NOT special-cased: it
    /// classifies as Fixed with an undecodable digest (rejected by the
    /// decode gates downstream), matching the default experimental-
    /// feature posture.
    #[test]
    fn impure_sentinel_is_a_fixed_output_with_junk_hash() {
        let o = DerivationOutput::new("out", P, "sha256", "impure").unwrap();
        assert!(matches!(o.kind(), OutputKind::Fixed { hash: "impure", .. }));
    }

    /// Junk algo names and junk digests remain representable — the
    /// gateway's realized-offender exemption flow carries them.
    #[test]
    fn junk_hash_values_remain_representable() {
        let o = DerivationOutput::new("out", P, "md5", "nothex!").unwrap();
        assert!(matches!(
            o.kind(),
            OutputKind::Fixed {
                hash_algo: "md5",
                hash: "nothex!",
                ..
            }
        ));
    }
}
