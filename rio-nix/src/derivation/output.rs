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
//! Four deliberate divergences from the oracle, all fail-closed:
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
//! 4. **Duplicate output names are rejected** at classification
//!    ([`DerivationTypeError::DuplicateOutputName`], the oracle EVAL's
//!    posture and verbatim wording, primops.cc:1529). The oracle's
//!    *parsers* instead first-wins-collapse duplicates into their
//!    `std::map` (`outputs.emplace`, derivations.cc:473 ATerm /
//!    derivations.cc:1001 wire) — but no honest `.drv` carries them
//!    (the eval refuses to emit one), and accepting a collapse would
//!    leave every downstream name-keyed view (validator zips, dispatch
//!    `position()` lookups, env-mask sets) silently partial over the
//!    positional storage. Rejecting makes name-keyed totality a parse
//!    fact.
//!
//! Junk hash *values* and junk algo *names* remain representable (raw
//! `String` fields) — the gateway's realized-offender exemption flow
//! requires carrying undecodable hashes through to its probe.

use crate::store_path::StorePath;

use super::DerivationError;

/// A single derivation output.
///
/// The classified shape is private (`OutputRepr`); consumers either
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
    // r[impl nix.divergence.output-path-parse]
    // r[impl nix.divergence.hash-without-algo]
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
    /// No-ops (returning `false`) on a non-deferred output instead of
    /// overwriting it: the previous `debug_assert!` + unconditional
    /// overwrite undercut the parse boundary's unrepresentability
    /// guarantee in release builds — a mis-targeted call would have
    /// silently rewritten a Fixed/Floating/concrete-IA output's shape,
    /// exactly the class the typed boundary exists to make unwritable.
    /// The sole caller (`fill_deferred_outputs`) pre-filters on
    /// `OutputKind::Deferred`, so a `false` return is a caller bug; it
    /// is surfaced there, not absorbed here.
    #[must_use]
    pub(crate) fn fill_deferred(&mut self, path: StorePath) -> bool {
        if !matches!(self.repr, OutputRepr::Deferred) {
            debug_assert!(false, "fill_deferred on a non-deferred output");
            return false;
        }
        self.repr = OutputRepr::InputAddressed { path };
        true
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
    // r[verify nix.divergence.output-path-parse]
    // r[verify nix.divergence.hash-without-algo]
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

/// The drv-level type of a derivation's output set (CppNix
/// `BasicDerivation::type()`, derivations.cc:795-854, minus `Impure` —
/// default experimental-feature posture).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DerivationType {
    /// All outputs input-addressed; `deferred` iff the paths are not
    /// yet known (every output Deferred — mixing concrete and deferred
    /// IA outputs is "can't mix", oracle parity).
    InputAddressed {
        /// Whether the IA paths are deferred.
        deferred: bool,
    },
    /// Single fixed output named "out".
    Fixed,
    /// All outputs floating-CA on one hash algorithm.
    Floating,
}

/// Ill-typed output sets, with the oracle's verbatim wording.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum DerivationTypeError {
    /// Outputs of different kinds in one derivation (including
    /// concrete-IA mixed with deferred-IA — distinct types in the
    /// oracle's `decide`).
    #[error("can't mix derivation output types")]
    Mixed,
    /// More than one fixed output.
    #[error("only one fixed output is allowed for now")]
    MultipleFixed,
    /// The single fixed output is not named "out".
    #[error("single fixed output must be named \"out\"")]
    FixedNotNamedOut,
    /// Floating outputs disagree on the hash algorithm (compared
    /// after stripping the `r:` method prefix — method is not part of
    /// the oracle's `hashAlgo`).
    #[error("all floating outputs must use the same hash algorithm")]
    FloatingAlgoMismatch,
    /// Empty output set.
    #[error("must have at least one output")]
    NoOutputs,
    /// Two outputs share a name. The oracle's eval refuses to construct
    /// this set (primops.cc:1529, wording verbatim); its parsers would
    /// first-wins-collapse it (module doc, divergence 4) — rio rejects
    /// fail-closed so name-keyed views stay total.
    #[error("duplicate derivation output '{0}'")]
    DuplicateOutputName(String),
}

/// The oracle's `hashAlgo` view of a floating algo string: the `r:`
/// method prefix is NOT part of the algorithm (`r:sha256` and `sha256`
/// are the same algorithm under different methods), while the
/// experimental `text:` prefix is compared raw — fail-closed, gated
/// upstream.
fn floating_algo(raw: &str) -> &str {
    raw.strip_prefix("r:").unwrap_or(raw)
}

/// Classify an output set, mirroring the oracle's `decide` fold over
/// its classification domain.
///
/// The oracle's `BasicDerivation::type()` iterates
/// `std::map<std::string, DerivationOutput>` — **byte-lexicographic
/// name order** — so *which* error an ill-typed set raises depends on
/// that order, not on wire/ATerm field order. rio stores outputs
/// positionally (byte-faithful round-trips are load-bearing), so the
/// fold here runs over a sorted *borrowed view*: same storage, same
/// classification domain as the oracle, variant selection independent
/// of input order.
///
/// Duplicate names are rejected before the kind fold (module doc,
/// divergence 4).
// r[impl nix.drv.type-classify+1]
pub fn classify_outputs(
    outputs: &[DerivationOutput],
) -> Result<DerivationType, DerivationTypeError> {
    // Sorted borrowed view == the oracle's std::map iteration order
    // (std::less<std::string> is a byte compare; Rust's str Ord is
    // too).
    let mut sorted: Vec<&DerivationOutput> = outputs.iter().collect();
    sorted.sort_by(|a, b| a.name().cmp(b.name()));
    // Window-scan duplicates over the sorted view BEFORE classifying:
    // a duplicate set has no oracle classification at all (the eval
    // never constructs one), so no kind error may win over this one.
    for w in sorted.windows(2) {
        if w[0].name() == w[1].name() {
            return Err(DerivationTypeError::DuplicateOutputName(
                w[0].name().to_owned(),
            ));
        }
    }
    let mut ty: Option<DerivationType> = None;
    let mut floating: Option<&str> = None;
    let decide = |new_ty: DerivationType, ty: &mut Option<DerivationType>| match *ty {
        None => {
            *ty = Some(new_ty);
            Ok(())
        }
        Some(t) if t != new_ty => Err(DerivationTypeError::Mixed),
        Some(DerivationType::Fixed) => Err(DerivationTypeError::MultipleFixed),
        Some(_) => Ok(()),
    };
    for o in sorted {
        match o.kind() {
            OutputKind::InputAddressed(_) => {
                decide(DerivationType::InputAddressed { deferred: false }, &mut ty)?;
            }
            OutputKind::Deferred => {
                decide(DerivationType::InputAddressed { deferred: true }, &mut ty)?;
            }
            OutputKind::Fixed { .. } => {
                decide(DerivationType::Fixed, &mut ty)?;
                if o.name() != "out" {
                    return Err(DerivationTypeError::FixedNotNamedOut);
                }
            }
            OutputKind::Floating { hash_algo } => {
                decide(DerivationType::Floating, &mut ty)?;
                let algo = floating_algo(hash_algo);
                match floating {
                    None => floating = Some(algo),
                    Some(prev) if prev != algo => {
                        return Err(DerivationTypeError::FloatingAlgoMismatch);
                    }
                    Some(_) => {}
                }
            }
        }
    }
    ty.ok_or(DerivationTypeError::NoOutputs)
}

#[cfg(test)]
mod classify_tests {
    use super::*;

    const P1: &str = "/nix/store/n2v52szmyja512fxmaax8lixl4dxh4jb-a";
    const P2: &str = "/nix/store/gjamk2f57j5pqymvqamgxla350szmld1-b";
    const H64: &str = "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";

    fn ia(name: &str, p: &str) -> DerivationOutput {
        DerivationOutput::new(name, p, "", "").unwrap()
    }
    fn deferred(name: &str) -> DerivationOutput {
        DerivationOutput::new(name, "", "", "").unwrap()
    }
    fn fixed(name: &str, p: &str) -> DerivationOutput {
        DerivationOutput::new(name, p, "sha256", H64).unwrap()
    }
    fn floating(name: &str, algo: &str) -> DerivationOutput {
        DerivationOutput::new(name, "", algo, "").unwrap()
    }

    /// Every legal uniform shape, including the `r:`-strip floating
    /// pair (same algorithm under different methods — the
    /// false-rejection trap).
    // r[verify nix.drv.type-classify+1]
    #[test]
    fn legal_shapes() {
        assert_eq!(
            classify_outputs(&[ia("out", P1), ia("dev", P2)]),
            Ok(DerivationType::InputAddressed { deferred: false })
        );
        assert_eq!(
            classify_outputs(&[deferred("out"), deferred("dev")]),
            Ok(DerivationType::InputAddressed { deferred: true })
        );
        assert_eq!(
            classify_outputs(&[fixed("out", P1)]),
            Ok(DerivationType::Fixed)
        );
        assert_eq!(
            classify_outputs(&[floating("out", "r:sha256"), floating("dev", "sha256")]),
            Ok(DerivationType::Floating)
        );
    }

    /// Every error variant, with the oracle's verbatim wording, over
    /// the oracle's truth table: error SELECTION follows the
    /// name-sorted classification domain (`std::map` order), not input
    /// order.
    // r[verify nix.drv.type-classify+1]
    #[test]
    fn error_matrix() {
        let cases: [(&[DerivationOutput], DerivationTypeError, &str); 7] = [
            (
                &[ia("out", P1), floating("dev", "sha256")],
                DerivationTypeError::Mixed,
                "can't mix derivation output types",
            ),
            // Concrete IA + deferred IA is ALSO a mix (distinct types
            // in the oracle's decide).
            (
                &[ia("out", P1), deferred("dev")],
                DerivationTypeError::Mixed,
                "can't mix derivation output types",
            ),
            // Sorted-domain truth table: "dev" < "out", so the oracle's
            // map fold sees the mis-named fixed output FIRST and throws
            // FixedNotNamedOut before it ever counts a second fixed
            // output. (rio's pre-sorted-domain fold over input order
            // returned MultipleFixed here — the merged_018 divergence.)
            (
                &[fixed("out", P1), fixed("dev", P2)],
                DerivationTypeError::FixedNotNamedOut,
                "single fixed output must be named \"out\"",
            ),
            // "out" < "zzz": the correctly-named fixed output decides
            // first, the second fixed output is the violation —
            // MultipleFixed, oracle-identical.
            (
                &[fixed("out", P1), fixed("zzz", P2)],
                DerivationTypeError::MultipleFixed,
                "only one fixed output is allowed for now",
            ),
            (
                &[fixed("src", P1)],
                DerivationTypeError::FixedNotNamedOut,
                "single fixed output must be named \"out\"",
            ),
            (
                &[floating("out", "sha256"), floating("dev", "sha512")],
                DerivationTypeError::FloatingAlgoMismatch,
                "all floating outputs must use the same hash algorithm",
            ),
            (
                &[],
                DerivationTypeError::NoOutputs,
                "must have at least one output",
            ),
        ];
        for (outs, want, wording) in cases {
            let err = classify_outputs(outs).unwrap_err();
            assert_eq!(err, want);
            assert_eq!(err.to_string(), wording, "oracle verbatim wording");
        }
        // text: floating prefixes compare raw (fail-closed).
        assert!(matches!(
            classify_outputs(&[floating("out", "text:sha256"), floating("dev", "sha256")]),
            Err(DerivationTypeError::FloatingAlgoMismatch)
        ));
    }

    /// Duplicate names are rejected with the oracle EVAL's verbatim
    /// wording (primops.cc:1529), and the rejection wins over every
    /// kind error: a duplicate set has no classification.
    ///
    /// `[fixed("out"), fixed("out")]` is the divergence-bearing shape:
    /// the oracle's PARSERS would first-wins-collapse it into a single
    /// legal fixed output (`outputs.emplace`, derivations.cc:473/1001)
    /// — accepted — while its eval refuses to construct it. rio sides
    /// with the eval, fail-closed (module doc, divergence 4).
    // r[verify nix.drv.type-classify+1]
    #[test]
    fn duplicate_matrix() {
        // Same shape twice (the parser-collapse divergence shape).
        let err = classify_outputs(&[fixed("out", P1), fixed("out", P2)]).unwrap_err();
        assert_eq!(err, DerivationTypeError::DuplicateOutputName("out".into()));
        assert_eq!(
            err.to_string(),
            "duplicate derivation output 'out'",
            "oracle eval verbatim wording"
        );
        // Duplicate beats Mixed.
        assert!(matches!(
            classify_outputs(&[ia("out", P1), floating("out", "sha256")]),
            Err(DerivationTypeError::DuplicateOutputName(n)) if n == "out"
        ));
        // Duplicate beats FloatingAlgoMismatch.
        assert!(matches!(
            classify_outputs(&[floating("out", "sha256"), floating("out", "sha512")]),
            Err(DerivationTypeError::DuplicateOutputName(n)) if n == "out"
        ));
        // Duplicate detected regardless of separation in input order.
        assert!(matches!(
            classify_outputs(&[ia("dev", P1), ia("out", P2), deferred("dev")]),
            Err(DerivationTypeError::DuplicateOutputName(n)) if n == "dev"
        ));
    }

    /// Variant selection is a function of the output SET — every
    /// permutation of the same outputs yields the identical `Result`
    /// (the fold domain is name-sorted, oracle `std::map` parity).
    /// Pins the sorted domain against an "optimization" that folds in
    /// input order again.
    // r[verify nix.drv.type-classify+1]
    #[test]
    fn classification_is_permutation_invariant() {
        fn permutations(n: usize) -> Vec<Vec<usize>> {
            if n == 0 {
                return vec![vec![]];
            }
            let mut out = Vec::new();
            for p in permutations(n - 1) {
                for i in 0..=p.len() {
                    let mut q = p.clone();
                    q.insert(i, n - 1);
                    out.push(q);
                }
            }
            out
        }
        const P3: &str = "/nix/store/vfhik20db6k5ff75sf3dbf6i3jymbnir-c";
        let sets: Vec<Vec<DerivationOutput>> = vec![
            // Legal uniform shapes.
            vec![ia("out", P1), ia("dev", P2), ia("lib", P3)],
            vec![floating("out", "r:sha256"), floating("dev", "sha256")],
            // Ill-typed: fixed not-named-out wins under sorted domain.
            vec![fixed("out", P1), fixed("dev", P2)],
            // Ill-typed: mixed kinds.
            vec![ia("out", P1), floating("dev", "sha256"), fixed("bin", P2)],
            // Duplicates.
            vec![fixed("out", P1), fixed("out", P2)],
            // Floating algo mismatch.
            vec![floating("out", "sha256"), floating("dev", "sha512")],
        ];
        for set in sets {
            let baseline = classify_outputs(&set);
            for perm in permutations(set.len()) {
                let reordered: Vec<DerivationOutput> =
                    perm.iter().map(|&i| set[i].clone()).collect();
                assert_eq!(
                    classify_outputs(&reordered),
                    baseline,
                    "order-dependent classification for permutation {perm:?}"
                );
            }
        }
    }
}
