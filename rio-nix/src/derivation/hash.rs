//! Modular derivation hashing (Nix `hashDerivationModulo`).

use std::collections::{BTreeMap, HashMap, HashSet};

use sha2::{Digest, Sha256};

use super::{Derivation, DerivationError, DerivationLike};
use crate::store_path::StorePath;

// ---------------------------------------------------------------------------
// hashDerivationModulo
// ---------------------------------------------------------------------------

/// Compute the modular derivation hash, matching Nix C++ `hashDerivationModulo`.
///
/// Three cases:
/// - **FOD** (fixed-output): `SHA-256("fixed:out:{hash_algo}:{hash}:{output_path}")`
/// - **Input-addressed**: replace `inputDrvs` keys with recursive modular hashes,
///   then `SHA-256(modified_aterm)`
/// - **CA floating / impure**: same as input-addressed but output paths are masked
///   to `""` in the ATerm
///
/// `resolve_input` maps a drv path string (e.g. `"/nix/store/1a4dmaqd1jgkj2kk6azvzqlvk8qvpq31.drv"`) to
/// the parsed `Derivation`. All transitive inputs must be resolvable.
///
/// `hash_cache` provides memoisation across calls (keyed by drv path string).
/// Only the `mask_outputs=false` form is cached — see the inner doc.
pub fn hash_derivation_modulo<'c>(
    drv: &'c Derivation,
    drv_path: &str,
    resolve_input: &dyn Fn(&str) -> Option<&'c Derivation>,
    hash_cache: &mut HashMap<String, [u8; 32]>,
) -> Result<[u8; 32], DerivationError> {
    // Nix 2.18-2.20: top-level entry (`staticOutputHashes`) passes
    // maskOutputs=true for CA-floating; recursive entry
    // (`pathDerivationModulo`) hard-codes false. We mirror that — the
    // top-level subject masks iff it is CA-floating; every input frame
    // inside the walk uses false.
    let mask_outputs = drv.has_ca_floating_outputs();
    hash_modulo_walk(drv, drv_path, resolve_input, hash_cache, mask_outputs)
}

/// The INPUT-position digest of a derivation: the `mask_outputs=false`
/// form CppNix's recursive `pathDerivationModulo` entry computes when a
/// derivation appears as another derivation's input.
///
/// For IA / FOD / deferred-IA subjects this equals
/// [`hash_derivation_modulo`] (no masking happens). For floating-CA
/// subjects the two DIVERGE: `hash_derivation_modulo` returns the
/// masked-subject form (the published hash, the realisation key), which
/// MUST NOT be used to stand in for the derivation in a consumer's
/// modulo walk. Callers that persist or seed per-drv hashes for input
/// resolution (e.g. the store's `drv_modulo_cache`) must use THIS form.
pub fn hash_derivation_modulo_input_form<'c>(
    drv: &'c Derivation,
    drv_path: &str,
    resolve_input: &dyn Fn(&str) -> Option<&'c Derivation>,
    hash_cache: &mut HashMap<String, [u8; 32]>,
) -> Result<[u8; 32], DerivationError> {
    hash_modulo_walk(drv, drv_path, resolve_input, hash_cache, false)
}

/// Canonicalize a declared FOD `outputHash` for the modulo fingerprint.
///
/// CppNix renders the fingerprint hash as
/// `dof.ca.hash.to_string(HashFormat::Base16, false)` (derivations.cc:904)
/// — canonical lowercase hex — regardless of which encoding the `.drv`
/// declared, so a nixbase32- or base64-declared hash must produce the SAME
/// modulo hash (and therefore the same realisation keys) as its base16
/// spelling.
///
/// Undecodable hashes (unsupported algorithm, junk digest) fall back to the
/// raw declared string. LOAD-BEARING: the gateway's offender-exemption flow
/// (`gw.reject.unsupported-hash-algo`) deliberately admits already-realized
/// FODs whose hashes rio cannot verify, and their fingerprints must stay
/// stable — failing here would turn an admission-policy decision into a
/// hash error deep inside the modulo walk.
// r[impl nix.hash.fod-decode]
fn canonical_fod_hash(raw_algo: &str, raw_hash: &str) -> String {
    use crate::hash::{HashAlgo, NixHash};

    raw_algo
        .strip_prefix("r:")
        .unwrap_or(raw_algo)
        .parse::<HashAlgo>()
        .ok()
        .and_then(|algo| NixHash::parse_nonsri_unprefixed(algo, raw_hash).ok())
        .map(|h| h.to_hex())
        .unwrap_or_else(|| raw_hash.to_owned())
}

/// One entry on the explicit traversal stack of [`hash_modulo_walk`].
enum WalkFrame<'c> {
    /// First touch of a derivation: memo/cycle checks, then push its
    /// `Finish` frame followed by `Visit` frames for its inputs.
    Visit { drv: &'c Derivation, path: String },
    /// All inputs are hashed (their `Visit` frames sat above this frame
    /// and have completed): assemble the rewrites and hash this node.
    Finish { drv: &'c Derivation, path: String },
}

/// Iterative post-order implementation of `hashDerivationModulo`.
///
/// `mask_subject` applies only to the top-level subject (`drv_path`): a
/// CA-floating drv has *two* distinct modular hashes — `mask=true` when
/// it is the realisation-key subject, `mask=false` when it appears as an
/// input to another drv. `hash_cache` stores only the `mask=false` value
/// (the form reused across input lookups), mirroring Nix where
/// `drvHashes` lives in `pathDerivationModulo`, not
/// `hashDerivationModulo`.
///
/// The walk is an explicit two-phase stack (`Visit`, then `Finish` once
/// the inputs are done) instead of recursion so the supported chain
/// depth is bounded by the closure-size caps alone, not by the thread's
/// stack — CppNix has no graph-depth limit and neither do we. The
/// `visiting` set holds exactly the paths whose `Finish` frame is still
/// pending (the ancestors of the node being expanded), which is the
/// same ancestor set the recursive form tracked, so cycle detection is
/// unchanged: re-visiting a pending path is a cycle, re-visiting a
/// completed (memoised) path is a cache hit.
fn hash_modulo_walk<'c>(
    subject_drv: &'c Derivation,
    subject_path: &str,
    resolve_input: &dyn Fn(&str) -> Option<&'c Derivation>,
    hash_cache: &mut HashMap<String, [u8; 32]>,
    mask_subject: bool,
) -> Result<[u8; 32], DerivationError> {
    // Memoisation fast path for the subject itself (mask=false form only).
    if !mask_subject && let Some(&cached) = hash_cache.get(subject_path) {
        return Ok(cached);
    }

    let mut visiting: HashSet<String> = HashSet::new();
    let mut stack: Vec<WalkFrame<'c>> = vec![WalkFrame::Visit {
        drv: subject_drv,
        path: subject_path.to_string(),
    }];
    let mut subject_hash: Option<[u8; 32]> = None;

    while let Some(frame) = stack.pop() {
        match frame {
            WalkFrame::Visit { drv, path } => {
                let is_subject = path == subject_path;
                let mask = is_subject && mask_subject;
                // Memoisation — cache holds the mask=false form only.
                // (Input frames are always mask=false; the subject may
                // re-appear as its own transitive input, in which case
                // the unmasked form is what that input position needs.)
                if !mask && hash_cache.contains_key(&path) {
                    continue;
                }
                // Cycle detection: the path's Finish frame is still
                // pending, i.e. it is an ancestor of itself.
                if visiting.contains(&path) {
                    return Err(DerivationError::CycleDetected(path));
                }
                visiting.insert(path.clone());

                if drv.is_fixed_output() {
                    // FOD base case: no inputs to expand.
                    stack.push(WalkFrame::Finish { drv, path });
                    continue;
                }
                // Expand inputs: their Visit frames sit ABOVE this
                // node's Finish frame, so they complete first
                // (post-order). Inputs are ALWAYS hashed with
                // mask_outputs=false (Nix `pathDerivationModulo`),
                // regardless of the input's own CA-floating-ness —
                // only the top-level subject masks.
                stack.push(WalkFrame::Finish { drv, path });
                for input_drv_path in drv.input_drvs().keys() {
                    if hash_cache.contains_key(input_drv_path) {
                        continue;
                    }
                    let input_drv = resolve_input(input_drv_path)
                        .ok_or_else(|| DerivationError::InputNotFound(input_drv_path.clone()))?;
                    stack.push(WalkFrame::Visit {
                        drv: input_drv,
                        path: input_drv_path.clone(),
                    });
                }
            }
            WalkFrame::Finish { drv, path } => {
                let is_subject = path == subject_path;
                let mask = is_subject && mask_subject;
                let hash: [u8; 32] = if drv.is_fixed_output() {
                    // FOD base case: hash the fingerprint string.
                    // Nix C++ derivations.cc:hashDerivationModulo — the
                    // output path IS part of the fingerprint. The
                    // trailing-colon-no-path shape was a copy-paste from
                    // store_path.rs make_store_path_hash where it IS
                    // correct (different function). See phase4a.md §5.
                    let output = &drv.outputs()[0];
                    let fingerprint = format!(
                        "fixed:out:{}:{}:{}",
                        output.hash_algo(),
                        canonical_fod_hash(output.hash_algo(), output.hash()),
                        output.path()
                    );
                    Sha256::digest(fingerprint.as_bytes()).into()
                } else {
                    let mut input_rewrites: BTreeMap<String, String> = BTreeMap::new();
                    for input_drv_path in drv.input_drvs().keys() {
                        // Every input completed before this Finish frame
                        // popped; a missing entry would have aborted the
                        // walk with InputNotFound during expansion.
                        let input_hash = hash_cache.get(input_drv_path).ok_or_else(|| {
                            DerivationError::InputNotFound(input_drv_path.clone())
                        })?;
                        input_rewrites.insert(input_drv_path.clone(), hex::encode(input_hash));
                    }
                    let modified_aterm = drv.to_aterm_modulo(&input_rewrites, mask)?;
                    Sha256::digest(modified_aterm.as_bytes()).into()
                };

                visiting.remove(&path);
                if !mask {
                    hash_cache.insert(path.clone(), hash);
                }
                if is_subject {
                    subject_hash = Some(hash);
                }
            }
        }
    }

    // The subject's Finish frame is pushed first and therefore pops
    // last; reaching here without it would be a walker bug.
    subject_hash.ok_or_else(|| DerivationError::InputNotFound(subject_path.to_string()))
}

// ---------------------------------------------------------------------------
// Input-addressed output path derivation (validation-side)
// ---------------------------------------------------------------------------

/// Derive the input-addressed output paths of `drv` from its contents,
/// ignoring whatever paths it declares.
///
/// This reproduces what Nix computes at instantiation time
/// (`derivationStrict` → `hashDerivationModulo(…, maskOutputs=true)` →
/// `StoreDirConfig::makeOutputPath`): every output path field and output
/// env value is masked to `""`, every `inputDrvs` key is replaced by that
/// input's modular hash (mask=false, exactly as the recursive arm of
/// [`hash_derivation_modulo`] does), the resulting ATerm is SHA-256
/// hashed, and each output's store path is
/// `makeOutputPath(outputName, hash, drvName)` where `drvName` is the
/// `.drv` store-path name without its extension.
///
/// Comparing the result against the declared paths is how a *trusted*
/// component (gateway / store) verifies that a submitted derivation does
/// not claim some other derivation's output path — workers are untrusted,
/// so any check they perform is defense-in-depth only.
///
/// Only meaningful for plain input-addressed derivations: fixed-output
/// derivations bind their path to the declared content hash
/// ([`StorePath::make_fixed_output`]) and floating-CA outputs have no
/// static path at all — both return [`DerivationError::NotInputAddressed`].
/// Deferred input-addressed outputs (empty declared path under `ca-derivations`)
/// are the caller's responsibility to skip: there is nothing to validate.
///
/// `hash_cache` memoises input modular hashes (mask=false form only) and
/// may be shared with [`hash_derivation_modulo`] calls over the same
/// closure.
pub fn input_addressed_output_paths<'c>(
    drv: &'c Derivation,
    drv_path: &str,
    resolve_input: &dyn Fn(&str) -> Option<&'c Derivation>,
    hash_cache: &mut HashMap<String, [u8; 32]>,
) -> Result<BTreeMap<String, StorePath>, DerivationError> {
    if drv.is_fixed_output() || drv.has_ca_floating_outputs() {
        return Err(DerivationError::NotInputAddressed(drv_path.to_string()));
    }

    // drvName: the `.drv` store path's name minus the extension — the same
    // value Nix uses for `outputPathName` when it first computes these
    // paths (a parsed `Derivation`'s `name` field is set from the path).
    let drv_store_path = StorePath::parse(drv_path)?;
    let drv_name = drv_store_path
        .name()
        .strip_suffix(".drv")
        .unwrap_or(drv_store_path.name())
        .to_owned();

    // Hash every input drv exactly the way the input frames of
    // `hash_derivation_modulo` do: mask=false — only the top-level
    // subject of the path computation is masked.
    //
    // Cache-first: a pre-seeded modulo hash for an input replaces the
    // need to resolve and walk that input at all (mirroring
    // `hash_modulo_walk`'s own cache-first input expansion). This is what
    // lets a caller compute IA output paths for a derivation whose
    // inputs it cannot resolve (no inline bytes, no store access) but
    // whose modulo hashes it knows from elsewhere — e.g. the scheduler
    // seeding the cache from sibling nodes' ingress-validated
    // `ca_modular_hash` values.
    //
    // Soundness (squat resistance): the derived output path is
    // `make_output(name, sha256(masked ATerm with input hashes
    // substituted), drv_name)`. A caller that seeds a WRONG hash for an
    // input does not gain the ability to produce someone else's path —
    // it moves the derived path into a namespace no honest resolver
    // computes (finding a seed that collides an honest path requires a
    // SHA-256 second preimage). Callers remain responsible for seeding
    // only hashes they have verified (the scheduler only seeds
    // ingress-validated values).
    let mut input_rewrites: BTreeMap<String, String> = BTreeMap::new();
    for input_drv_path in drv.input_drvs().keys() {
        if let Some(cached) = hash_cache.get(input_drv_path) {
            input_rewrites.insert(input_drv_path.clone(), hex::encode(cached));
            continue;
        }
        let input_drv = resolve_input(input_drv_path)
            .ok_or_else(|| DerivationError::InputNotFound(input_drv_path.clone()))?;
        let input_hash =
            hash_modulo_walk(input_drv, input_drv_path, resolve_input, hash_cache, false)?;
        input_rewrites.insert(input_drv_path.clone(), hex::encode(input_hash));
    }

    // Mask the outputs (path fields AND output-named env values) the way
    // Nix does while the paths are still unknown, then hash the ATerm.
    let masked_aterm = drv.to_aterm_modulo(&input_rewrites, true)?;
    let drv_hash: [u8; 32] = Sha256::digest(masked_aterm.as_bytes()).into();

    let mut paths = BTreeMap::new();
    for output in drv.outputs() {
        let path = StorePath::make_output(output.name(), &drv_hash, &drv_name)?;
        paths.insert(output.name().to_owned(), path);
    }
    Ok(paths)
}

#[cfg(test)]
mod hash_derivation_modulo_tests {
    use super::*;
    use std::collections::{BTreeMap, HashMap};

    /// Helper: create a simple input-addressed derivation (no inputDrvs).
    fn leaf_ia_drv() -> Derivation {
        Derivation::parse(
                r#"Derive([("out","/nix/store/1a4dmaqd1jgkj2kk6azvzqlvk8qvpq31-leaf","","")],[],[],"x86_64-linux","/bin/sh",["-c","echo hello"],[("name","leaf"),("out","/nix/store/1a4dmaqd1jgkj2kk6azvzqlvk8qvpq31-leaf"),("system","x86_64-linux")])"#,
            ).expect("static fixture")
    }

    /// Helper: create a fixed-output derivation.
    fn fod_drv() -> Derivation {
        Derivation::parse(
                r#"Derive([("out","/nix/store/hf9x46xx06qmkj0ivfqdswgi2qzd2cwz-fixed","sha256","e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855")],[],[],"x86_64-linux","/bin/sh",["-c","echo"],[("name","fixed"),("out","/nix/store/hf9x46xx06qmkj0ivfqdswgi2qzd2cwz-fixed"),("outputHash","e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"),("outputHashAlgo","sha256"),("system","x86_64-linux")])"#,
            ).expect("static fixture")
    }

    /// Helper: create an IA derivation that depends on the FOD.
    fn ia_with_fod_input() -> Derivation {
        Derivation::parse(
                r#"Derive([("out","/nix/store/lfmx061x46cmzac9zywx6lb1yhvxyl8z-dependent","","")],[("/nix/store/hf9x46xx06qmkj0ivfqdswgi2qzd2cwz-fixed.drv",["out"])],[],"x86_64-linux","/bin/sh",["-c","echo"],[("name","dependent"),("out","/nix/store/lfmx061x46cmzac9zywx6lb1yhvxyl8z-dependent"),("system","x86_64-linux")])"#,
            ).expect("static fixture")
    }

    #[test]
    fn fod_hash_matches_fingerprint() -> anyhow::Result<()> {
        use sha2::{Digest, Sha256};

        let drv = fod_drv();
        assert!(drv.is_fixed_output());

        let mut cache = HashMap::new();
        let resolve = |_: &str| -> Option<&Derivation> { None };
        let hash = hash_derivation_modulo(
            &drv,
            "/nix/store/hf9x46xx06qmkj0ivfqdswgi2qzd2cwz-fixed.drv",
            &resolve,
            &mut cache,
        )?;

        // Expected: SHA-256("fixed:out:sha256:<hex>:/nix/store/hf9x46xx06qmkj0ivfqdswgi2qzd2cwz-fixed")
        let expected: [u8; 32] = Sha256::digest(
            b"fixed:out:sha256:e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855:/nix/store/hf9x46xx06qmkj0ivfqdswgi2qzd2cwz-fixed",
        )
        .into();

        assert_eq!(hash, expected);
        Ok(())
    }

    /// CppNix parity: the modulo fingerprint canonicalizes the declared
    /// outputHash to lowercase hex (derivations.cc:904 renders
    /// `HashFormat::Base16`), so the same digest declared in base16,
    /// nixbase32, or base64 yields the SAME modulo hash — and therefore
    /// the same realisation keys.
    // r[verify nix.hash.fod-decode]
    #[test]
    fn fod_hash_canonicalizes_declared_encoding() -> anyhow::Result<()> {
        use base64::Engine;
        use sha2::{Digest, Sha256};

        use crate::store_path::nixbase32;

        let digest_hex = "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";
        let digest = hex::decode(digest_hex)?;
        let declared = [
            digest_hex.to_owned(),
            nixbase32::encode(&digest),
            base64::engine::general_purpose::STANDARD.encode(&digest),
        ];

        // All three encodings must produce the fingerprint built from the
        // canonical hex rendering.
        let expected: [u8; 32] = Sha256::digest(format!(
            "fixed:out:sha256:{digest_hex}:/nix/store/hf9x46xx06qmkj0ivfqdswgi2qzd2cwz-fixed"
        ))
        .into();

        for declared_hash in declared {
            let aterm = format!(
                r#"Derive([("out","/nix/store/hf9x46xx06qmkj0ivfqdswgi2qzd2cwz-fixed","sha256","{declared_hash}")],[],[],"x86_64-linux","/bin/sh",["-c","echo"],[("name","fixed"),("out","/nix/store/hf9x46xx06qmkj0ivfqdswgi2qzd2cwz-fixed"),("outputHash","{declared_hash}"),("outputHashAlgo","sha256"),("system","x86_64-linux")])"#
            );
            let drv = Derivation::parse(&aterm)?;
            assert!(drv.is_fixed_output());

            let mut cache = HashMap::new();
            let resolve = |_: &str| -> Option<&Derivation> { None };
            let hash = hash_derivation_modulo(
                &drv,
                "/nix/store/hf9x46xx06qmkj0ivfqdswgi2qzd2cwz-fixed.drv",
                &resolve,
                &mut cache,
            )?;
            assert_eq!(
                hash, expected,
                "declared encoding {declared_hash:?} must canonicalize to the hex fingerprint"
            );
        }
        Ok(())
    }

    /// An undecodable declared hash (unsupported algo, junk digest) keeps
    /// the raw string in the fingerprint — stable across versions, never an
    /// error. The gateway's offender-exemption flow depends on this.
    // r[verify nix.hash.fod-decode]
    #[test]
    fn fod_hash_undecodable_falls_back_to_raw() -> anyhow::Result<()> {
        use sha2::{Digest, Sha256};

        // md5 is not a supported HashAlgo; the raw declared string must be
        // used verbatim.
        let aterm = r#"Derive([("out","/nix/store/hf9x46xx06qmkj0ivfqdswgi2qzd2cwz-fixed","md5","0123456789abcdef0123456789abcdef")],[],[],"x86_64-linux","/bin/sh",["-c","echo"],[("name","fixed"),("out","/nix/store/hf9x46xx06qmkj0ivfqdswgi2qzd2cwz-fixed"),("outputHash","0123456789abcdef0123456789abcdef"),("outputHashAlgo","md5"),("system","x86_64-linux")])"#;
        let drv = Derivation::parse(aterm)?;

        let mut cache = HashMap::new();
        let resolve = |_: &str| -> Option<&Derivation> { None };
        let hash = hash_derivation_modulo(
            &drv,
            "/nix/store/hf9x46xx06qmkj0ivfqdswgi2qzd2cwz-fixed.drv",
            &resolve,
            &mut cache,
        )?;

        let expected: [u8; 32] =
            Sha256::digest(b"fixed:out:md5:0123456789abcdef0123456789abcdef:/nix/store/hf9x46xx06qmkj0ivfqdswgi2qzd2cwz-fixed")
                .into();
        assert_eq!(hash, expected);

        // A wrong-length sha256 digest also falls back to raw.
        let aterm_junk = r#"Derive([("out","/nix/store/hf9x46xx06qmkj0ivfqdswgi2qzd2cwz-fixed","sha256","deadbeef")],[],[],"x86_64-linux","/bin/sh",["-c","echo"],[("name","fixed"),("out","/nix/store/hf9x46xx06qmkj0ivfqdswgi2qzd2cwz-fixed"),("outputHash","deadbeef"),("outputHashAlgo","sha256"),("system","x86_64-linux")])"#;
        let drv_junk = Derivation::parse(aterm_junk)?;
        let mut cache2 = HashMap::new();
        let hash_junk = hash_derivation_modulo(
            &drv_junk,
            "/nix/store/hf9x46xx06qmkj0ivfqdswgi2qzd2cwz-fixed.drv",
            &resolve,
            &mut cache2,
        )?;
        let expected_junk: [u8; 32] = Sha256::digest(
            b"fixed:out:sha256:deadbeef:/nix/store/hf9x46xx06qmkj0ivfqdswgi2qzd2cwz-fixed",
        )
        .into();
        assert_eq!(hash_junk, expected_junk);
        Ok(())
    }

    #[test]
    fn leaf_ia_hash_equals_aterm_hash() -> anyhow::Result<()> {
        use sha2::{Digest, Sha256};

        let drv = leaf_ia_drv();
        assert!(!drv.is_fixed_output());
        assert!(!drv.has_ca_floating_outputs());

        let mut cache = HashMap::new();
        let resolve = |_: &str| -> Option<&Derivation> { None };
        let hash = hash_derivation_modulo(
            &drv,
            "/nix/store/1a4dmaqd1jgkj2kk6azvzqlvk8qvpq31-leaf.drv",
            &resolve,
            &mut cache,
        )?;

        // Leaf IA with no inputDrvs: to_aterm_modulo(empty, false) == to_aterm()
        let expected: [u8; 32] = Sha256::digest(drv.to_aterm().as_bytes()).into();
        assert_eq!(hash, expected);
        Ok(())
    }

    #[test]
    fn ia_with_fod_input_uses_modular_hash() -> anyhow::Result<()> {
        use sha2::{Digest, Sha256};

        let fod = fod_drv();
        let dep = ia_with_fod_input();

        let mut cache = HashMap::new();
        let resolve = |path: &str| -> Option<&Derivation> {
            if path == "/nix/store/hf9x46xx06qmkj0ivfqdswgi2qzd2cwz-fixed.drv" {
                Some(&fod)
            } else {
                None
            }
        };
        let hash = hash_derivation_modulo(
            &dep,
            "/nix/store/lfmx061x46cmzac9zywx6lb1yhvxyl8z-dependent.drv",
            &resolve,
            &mut cache,
        )?;

        // The FOD modular hash
        let fod_hash: [u8; 32] = Sha256::digest(
            b"fixed:out:sha256:e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855:/nix/store/hf9x46xx06qmkj0ivfqdswgi2qzd2cwz-fixed",
        )
        .into();
        let fod_hex = hex::encode(fod_hash);

        // The modified ATerm should have the FOD hex hash instead of the drv path
        let mut rewrites = BTreeMap::new();
        rewrites.insert(
            "/nix/store/hf9x46xx06qmkj0ivfqdswgi2qzd2cwz-fixed.drv".to_string(),
            fod_hex.clone(),
        );
        let modified_aterm = dep.to_aterm_modulo(&rewrites, false)?;

        // Verify the modified ATerm contains the hex hash, not the drv path
        assert!(modified_aterm.contains(&fod_hex));
        assert!(!modified_aterm.contains("/nix/store/hf9x46xx06qmkj0ivfqdswgi2qzd2cwz-fixed.drv"));

        let expected: [u8; 32] = Sha256::digest(modified_aterm.as_bytes()).into();
        assert_eq!(hash, expected);
        Ok(())
    }

    #[test]
    fn chained_ia_depth_2() -> anyhow::Result<()> {
        // Chain: C depends on B depends on FOD A
        let fod_a = fod_drv();
        let b = ia_with_fod_input(); // depends on FOD
        let c = Derivation::parse(
            r#"Derive([("out","/nix/store/bj25pjndbamvhnxz8advdq6p5cf3xhir-chain","","")],[("/nix/store/lfmx061x46cmzac9zywx6lb1yhvxyl8z-dependent.drv",["out"])],[],"x86_64-linux","/bin/sh",["-c","echo"],[("name","chain"),("out","/nix/store/bj25pjndbamvhnxz8advdq6p5cf3xhir-chain"),("system","x86_64-linux")])"#,
        )?;

        let mut cache = HashMap::new();
        let resolve = |path: &str| -> Option<&Derivation> {
            match path {
                "/nix/store/hf9x46xx06qmkj0ivfqdswgi2qzd2cwz-fixed.drv" => Some(&fod_a),
                "/nix/store/lfmx061x46cmzac9zywx6lb1yhvxyl8z-dependent.drv" => Some(&b),
                _ => None,
            }
        };

        let hash = hash_derivation_modulo(
            &c,
            "/nix/store/bj25pjndbamvhnxz8advdq6p5cf3xhir-chain.drv",
            &resolve,
            &mut cache,
        )?;

        // Both A and B should now be cached
        assert!(cache.contains_key("/nix/store/hf9x46xx06qmkj0ivfqdswgi2qzd2cwz-fixed.drv"));
        assert!(cache.contains_key("/nix/store/lfmx061x46cmzac9zywx6lb1yhvxyl8z-dependent.drv"));
        assert!(cache.contains_key("/nix/store/bj25pjndbamvhnxz8advdq6p5cf3xhir-chain.drv"));

        // Verify determinism: computing again gives same result
        let hash2 = hash_derivation_modulo(
            &c,
            "/nix/store/bj25pjndbamvhnxz8advdq6p5cf3xhir-chain.drv",
            &resolve,
            &mut cache,
        )?;
        assert_eq!(hash, hash2);
        Ok(())
    }

    #[test]
    fn multi_output_with_outputs_from_drv() -> anyhow::Result<()> {
        use crate::protocol::build::{BuildResult, BuildStatus};

        let drv = Derivation::parse(
            r#"Derive([("dev","/nix/store/1a4dmaqd1jgkj2kk6azvzqlvk8qvpq31-dev","",""),("lib","/nix/store/1a4dmaqd1jgkj2kk6azvzqlvk8qvpq31-lib","",""),("out","/nix/store/1a4dmaqd1jgkj2kk6azvzqlvk8qvpq31-out","","")],[],[],"x86_64-linux","/bin/sh",["-c","echo"],[("name","multi"),("out","/nix/store/1a4dmaqd1jgkj2kk6azvzqlvk8qvpq31-out"),("system","x86_64-linux")])"#,
        )?;

        let mut cache = HashMap::new();
        let resolve = |_: &str| -> Option<&Derivation> { None };
        let hash = hash_derivation_modulo(
            &drv,
            "/nix/store/1a4dmaqd1jgkj2kk6azvzqlvk8qvpq31.drv",
            &resolve,
            &mut cache,
        )?;
        let drv_hash_hex = hex::encode(hash);

        let result =
            BuildResult::success().with_outputs_from_drv(&drv, &drv_hash_hex, &HashMap::new());
        assert_eq!(result.status, BuildStatus::Built);
        assert_eq!(result.built_outputs.len(), 3);

        // Outputs should be in derivation order with correct IDs and paths
        assert_eq!(
            result.built_outputs[0].drv_output_id,
            format!("sha256:{drv_hash_hex}!dev")
        );
        assert_eq!(
            result.built_outputs[0].out_path,
            "/nix/store/1a4dmaqd1jgkj2kk6azvzqlvk8qvpq31-dev"
        );

        assert_eq!(
            result.built_outputs[1].drv_output_id,
            format!("sha256:{drv_hash_hex}!lib")
        );
        assert_eq!(
            result.built_outputs[1].out_path,
            "/nix/store/1a4dmaqd1jgkj2kk6azvzqlvk8qvpq31-lib"
        );

        assert_eq!(
            result.built_outputs[2].drv_output_id,
            format!("sha256:{drv_hash_hex}!out")
        );
        assert_eq!(
            result.built_outputs[2].out_path,
            "/nix/store/1a4dmaqd1jgkj2kk6azvzqlvk8qvpq31-out"
        );
        Ok(())
    }

    #[test]
    fn ca_floating_masks_output_paths() -> anyhow::Result<()> {
        // CA floating: hash_algo set, hash empty
        let ca_drv = Derivation::parse(
            r#"Derive([("out","/nix/store/yybiyabxfnjrmn69rna94qxr881rwqfv-ca","sha256","")],[],[],"x86_64-linux","/bin/sh",["-c","echo"],[("name","ca-test"),("out","/nix/store/yybiyabxfnjrmn69rna94qxr881rwqfv-ca"),("system","x86_64-linux")])"#,
        )?;

        assert!(ca_drv.has_ca_floating_outputs());
        assert!(!ca_drv.is_fixed_output()); // hash is empty, so not FOD

        let mut cache = HashMap::new();
        let resolve = |_: &str| -> Option<&Derivation> { None };
        let hash = hash_derivation_modulo(
            &ca_drv,
            "/nix/store/vfhik20db6k5ff75sf3dbf6i3jymbnir.drv",
            &resolve,
            &mut cache,
        )?;

        // Verify the hash uses masked ATerm (empty output path AND empty
        // env value for the `out` key — Nix C++ masks both; missing the
        // env mask produces a hash that nix-build's wopQueryRealisation
        // will never match).
        use sha2::{Digest, Sha256};
        let masked_aterm = ca_drv.to_aterm_modulo(&BTreeMap::new(), true)?;
        assert!(masked_aterm.contains(r#"("out","","sha256","")"#));
        // Env `out` value masked: original fixture has
        // ("out","/nix/store/yybiyabxfnjrmn69rna94qxr881rwqfv-ca"), masked has ("out","").
        assert!(
            masked_aterm.contains(r#"("out","")"#),
            "env out var should be masked to empty; got: {masked_aterm}"
        );
        assert!(
            !masked_aterm.contains("/nix/store/yybiyabxfnjrmn69rna94qxr881rwqfv-ca"),
            "placeholder path should not appear anywhere after masking"
        );
        let expected: [u8; 32] = Sha256::digest(masked_aterm.as_bytes()).into();
        assert_eq!(hash, expected);
        Ok(())
    }

    #[test]
    fn memoization_works() -> anyhow::Result<()> {
        let drv = leaf_ia_drv();
        let mut cache = HashMap::new();
        let resolve = |_: &str| -> Option<&Derivation> { None };

        let hash1 = hash_derivation_modulo(
            &drv,
            "/nix/store/1a4dmaqd1jgkj2kk6azvzqlvk8qvpq31-leaf.drv",
            &resolve,
            &mut cache,
        )?;
        assert_eq!(cache.len(), 1);
        assert!(cache.contains_key("/nix/store/1a4dmaqd1jgkj2kk6azvzqlvk8qvpq31-leaf.drv"));

        let hash2 = hash_derivation_modulo(
            &drv,
            "/nix/store/1a4dmaqd1jgkj2kk6azvzqlvk8qvpq31-leaf.drv",
            &resolve,
            &mut cache,
        )?;
        assert_eq!(hash1, hash2);
        Ok(())
    }

    #[test]
    fn missing_input_returns_error() {
        let dep = ia_with_fod_input(); // depends on /nix/store/hf9x46xx06qmkj0ivfqdswgi2qzd2cwz-fixed.drv
        let mut cache = HashMap::new();
        let resolve = |_: &str| -> Option<&Derivation> { None }; // no inputs available

        let result = hash_derivation_modulo(
            &dep,
            "/nix/store/jsgmnakmssnxxajxi2n5lld58c9mcly3.drv",
            &resolve,
            &mut cache,
        );
        assert!(matches!(result, Err(DerivationError::InputNotFound(_))));
    }

    #[test]
    fn cycle_detection() -> anyhow::Result<()> {
        // Create a derivation that references itself
        let cyclic = Derivation::parse(
            r#"Derive([("out","/nix/store/1a4dmaqd1jgkj2kk6azvzqlvk8qvpq31-cyclic","","")],[("/nix/store/1a4dmaqd1jgkj2kk6azvzqlvk8qvpq31-cyclic.drv",["out"])],[],"x86_64-linux","/bin/sh",["-c","echo"],[("name","cyclic"),("out","/nix/store/1a4dmaqd1jgkj2kk6azvzqlvk8qvpq31-cyclic"),("system","x86_64-linux")])"#,
        )?;

        let mut cache = HashMap::new();
        let resolve = |path: &str| -> Option<&Derivation> {
            if path == "/nix/store/1a4dmaqd1jgkj2kk6azvzqlvk8qvpq31-cyclic.drv" {
                Some(&cyclic)
            } else {
                None
            }
        };

        let result = hash_derivation_modulo(
            &cyclic,
            "/nix/store/1a4dmaqd1jgkj2kk6azvzqlvk8qvpq31-cyclic.drv",
            &resolve,
            &mut cache,
        );
        assert!(matches!(result, Err(DerivationError::CycleDetected(_))));
        Ok(())
    }

    #[test]
    fn to_aterm_modulo_replaces_input_keys() -> anyhow::Result<()> {
        let drv = Derivation::parse(
            r#"Derive([("out","/nix/store/1a4dmaqd1jgkj2kk6azvzqlvk8qvpq31-test","","")],[("/nix/store/n2v52szmyja512fxmaax8lixl4dxh4jb-input.drv",["out"]),("/nix/store/gjamk2f57j5pqymvqamgxla350szmld1-input.drv",["dev","out"])],[],"x86_64-linux","/bin/sh",[],[("name","test"),("system","x86_64-linux")])"#,
        )?;

        let mut rewrites = BTreeMap::new();
        rewrites.insert(
            "/nix/store/n2v52szmyja512fxmaax8lixl4dxh4jb-input.drv".to_string(),
            "aaaa".repeat(16), // 64-char hex
        );
        rewrites.insert(
            "/nix/store/gjamk2f57j5pqymvqamgxla350szmld1-input.drv".to_string(),
            "bbbb".repeat(16),
        );

        let result = drv.to_aterm_modulo(&rewrites, false)?;

        // Keys should be the hex hashes, not the drv paths
        assert!(result.contains(&"aaaa".repeat(16)));
        assert!(result.contains(&"bbbb".repeat(16)));
        assert!(!result.contains("/nix/store/n2v52szmyja512fxmaax8lixl4dxh4jb-input.drv"));
        assert!(!result.contains("/nix/store/gjamk2f57j5pqymvqamgxla350szmld1-input.drv"));

        // Output paths should be preserved (mask_outputs=false)
        assert!(result.contains("/nix/store/1a4dmaqd1jgkj2kk6azvzqlvk8qvpq31-test"));
        Ok(())
    }

    #[test]
    fn to_aterm_modulo_sorts_by_replacement_keys() -> anyhow::Result<()> {
        // Original keys sorted: aaa < bbb
        // Replacement keys sorted: zzzz > aaaa (reversed!)
        let drv = Derivation::parse(
            r#"Derive([("out","/nix/store/1a4dmaqd1jgkj2kk6azvzqlvk8qvpq31-test","","")],[("/nix/store/n2v52szmyja512fxmaax8lixl4dxh4jb.drv",["out"]),("/nix/store/gjamk2f57j5pqymvqamgxla350szmld1.drv",["out"])],[],"x86_64-linux","/bin/sh",[],[("name","test"),("system","x86_64-linux")])"#,
        )?;

        let mut rewrites = BTreeMap::new();
        rewrites.insert(
            "/nix/store/n2v52szmyja512fxmaax8lixl4dxh4jb.drv".to_string(),
            "z".repeat(64), // sorts AFTER b...
        );
        rewrites.insert(
            "/nix/store/gjamk2f57j5pqymvqamgxla350szmld1.drv".to_string(),
            "a".repeat(64), // sorts BEFORE z...
        );

        let result = drv.to_aterm_modulo(&rewrites, false)?;

        // In the ATerm, the "a" key should appear before the "z" key
        let a_pos = result.find(&"a".repeat(64)).unwrap();
        let z_pos = result.find(&"z".repeat(64)).unwrap();
        assert!(
            a_pos < z_pos,
            "replacement keys must be sorted in the ATerm"
        );
        Ok(())
    }

    #[test]
    fn fod_recursive_hash() -> anyhow::Result<()> {
        use sha2::{Digest, Sha256};

        // Recursive FOD: hash_algo = "r:sha256"
        let drv = Derivation::parse(
            r#"Derive([("out","/nix/store/hf9x46xx06qmkj0ivfqdswgi2qzd2cwz-rec","r:sha256","e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855")],[],[],"x86_64-linux","/bin/sh",["-c","echo"],[("name","rec"),("out","/nix/store/hf9x46xx06qmkj0ivfqdswgi2qzd2cwz-rec"),("outputHash","e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"),("outputHashAlgo","r:sha256"),("system","x86_64-linux")])"#,
        )?;

        assert!(drv.is_fixed_output());

        let mut cache = HashMap::new();
        let resolve = |_: &str| -> Option<&Derivation> { None };
        let hash = hash_derivation_modulo(
            &drv,
            "/nix/store/hf9x46xx06qmkj0ivfqdswgi2qzd2cwz-rec.drv",
            &resolve,
            &mut cache,
        )?;

        // Expected: SHA-256("fixed:out:r:sha256:<hex>:/nix/store/hf9x46xx06qmkj0ivfqdswgi2qzd2cwz-rec")
        let expected: [u8; 32] = Sha256::digest(
            b"fixed:out:r:sha256:e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855:/nix/store/hf9x46xx06qmkj0ivfqdswgi2qzd2cwz-rec",
        )
        .into();

        assert_eq!(hash, expected);
        Ok(())
    }

    /// Two FODs with identical (algo, hash) but different output paths must
    /// produce different modular hashes — nix-aterm-modulo-key-collision.
    /// Before this fix they collided because the fingerprint omitted path.
    #[test]
    fn fod_different_paths_different_hashes() -> anyhow::Result<()> {
        let drv_a = Derivation::parse(
            r#"Derive([("out","/nix/store/n2v52szmyja512fxmaax8lixl4dxh4jb-fixed","sha256","e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855")],[],[],"x86_64-linux","/bin/sh",["-c","echo"],[("name","fixed-a"),("out","/nix/store/n2v52szmyja512fxmaax8lixl4dxh4jb-fixed"),("outputHash","e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"),("outputHashAlgo","sha256"),("system","x86_64-linux")])"#,
        )?;
        let drv_b = Derivation::parse(
            r#"Derive([("out","/nix/store/gjamk2f57j5pqymvqamgxla350szmld1-fixed","sha256","e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855")],[],[],"x86_64-linux","/bin/sh",["-c","echo"],[("name","fixed-b"),("out","/nix/store/gjamk2f57j5pqymvqamgxla350szmld1-fixed"),("outputHash","e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"),("outputHashAlgo","sha256"),("system","x86_64-linux")])"#,
        )?;

        let mut cache = HashMap::new();
        let resolve = |_: &str| -> Option<&Derivation> { None };
        let hash_a = hash_derivation_modulo(
            &drv_a,
            "/nix/store/n2v52szmyja512fxmaax8lixl4dxh4jb-fixed.drv",
            &resolve,
            &mut cache,
        )?;
        let hash_b = hash_derivation_modulo(
            &drv_b,
            "/nix/store/gjamk2f57j5pqymvqamgxla350szmld1-fixed.drv",
            &resolve,
            &mut cache,
        )?;

        assert_ne!(
            hash_a, hash_b,
            "FODs differing only in output path must not collide"
        );
        Ok(())
    }

    #[test]
    fn diamond_dag_memoization() -> anyhow::Result<()> {
        // Diamond: D depends on B and C, both depend on FOD A
        let a = fod_drv();
        let b = ia_with_fod_input(); // depends on /nix/store/hf9x46xx06qmkj0ivfqdswgi2qzd2cwz-fixed.drv
        let c = Derivation::parse(
            r#"Derive([("out","/nix/store/h215ws5mqjq1pnqd7j0incvdyqk96lhp-other","","")],[("/nix/store/hf9x46xx06qmkj0ivfqdswgi2qzd2cwz-fixed.drv",["out"])],[],"x86_64-linux","/bin/sh",["-c","echo"],[("name","other"),("out","/nix/store/h215ws5mqjq1pnqd7j0incvdyqk96lhp-other"),("system","x86_64-linux")])"#,
        )?;
        let d = Derivation::parse(
            r#"Derive([("out","/nix/store/9jhm52c51jvgn2p7a2kzv44pg4q7bwv1-diamond","","")],[("/nix/store/h215ws5mqjq1pnqd7j0incvdyqk96lhp-other.drv",["out"]),("/nix/store/lfmx061x46cmzac9zywx6lb1yhvxyl8z-dependent.drv",["out"])],[],"x86_64-linux","/bin/sh",["-c","echo"],[("name","diamond"),("out","/nix/store/9jhm52c51jvgn2p7a2kzv44pg4q7bwv1-diamond"),("system","x86_64-linux")])"#,
        )?;

        let mut cache = HashMap::new();
        let resolve = |path: &str| -> Option<&Derivation> {
            match path {
                "/nix/store/hf9x46xx06qmkj0ivfqdswgi2qzd2cwz-fixed.drv" => Some(&a),
                "/nix/store/lfmx061x46cmzac9zywx6lb1yhvxyl8z-dependent.drv" => Some(&b),
                "/nix/store/h215ws5mqjq1pnqd7j0incvdyqk96lhp-other.drv" => Some(&c),
                _ => None,
            }
        };

        // Should not produce a false CycleDetected for the shared FOD A
        let hash = hash_derivation_modulo(
            &d,
            "/nix/store/9jhm52c51jvgn2p7a2kzv44pg4q7bwv1-diamond.drv",
            &resolve,
            &mut cache,
        )?;

        // All 4 should be cached
        assert_eq!(cache.len(), 4);
        assert!(cache.contains_key("/nix/store/hf9x46xx06qmkj0ivfqdswgi2qzd2cwz-fixed.drv"));

        // Determinism check
        let hash2 = hash_derivation_modulo(
            &d,
            "/nix/store/9jhm52c51jvgn2p7a2kzv44pg4q7bwv1-diamond.drv",
            &resolve,
            &mut cache,
        )?;
        assert_eq!(hash, hash2);
        Ok(())
    }

    #[test]
    fn indirect_cycle_detection() -> anyhow::Result<()> {
        // A -> B -> A (indirect cycle)
        let a = Derivation::parse(
            r#"Derive([("out","/nix/store/n2v52szmyja512fxmaax8lixl4dxh4jb-cycle","","")],[("/nix/store/gjamk2f57j5pqymvqamgxla350szmld1-cycle.drv",["out"])],[],"x86_64-linux","/bin/sh",["-c","echo"],[("name","a"),("system","x86_64-linux")])"#,
        )?;
        let b = Derivation::parse(
            r#"Derive([("out","/nix/store/gjamk2f57j5pqymvqamgxla350szmld1-cycle","","")],[("/nix/store/n2v52szmyja512fxmaax8lixl4dxh4jb-cycle.drv",["out"])],[],"x86_64-linux","/bin/sh",["-c","echo"],[("name","b"),("system","x86_64-linux")])"#,
        )?;

        let mut cache = HashMap::new();
        let resolve = |path: &str| -> Option<&Derivation> {
            match path {
                "/nix/store/n2v52szmyja512fxmaax8lixl4dxh4jb-cycle.drv" => Some(&a),
                "/nix/store/gjamk2f57j5pqymvqamgxla350szmld1-cycle.drv" => Some(&b),
                _ => None,
            }
        };

        let result = hash_derivation_modulo(
            &a,
            "/nix/store/n2v52szmyja512fxmaax8lixl4dxh4jb-cycle.drv",
            &resolve,
            &mut cache,
        );
        assert!(matches!(result, Err(DerivationError::CycleDetected(_))));
        Ok(())
    }

    #[test]
    fn with_outputs_from_drv_produces_correct_ids() -> anyhow::Result<()> {
        use crate::protocol::build::{BuildResult, BuildStatus};

        let drv = fod_drv(); // FOD with known hash
        let mut cache = HashMap::new();
        let resolve = |_: &str| -> Option<&Derivation> { None };
        let hash = hash_derivation_modulo(
            &drv,
            "/nix/store/hf9x46xx06qmkj0ivfqdswgi2qzd2cwz-fixed.drv",
            &resolve,
            &mut cache,
        )?;
        let drv_hash_hex = hex::encode(hash);

        let result =
            BuildResult::success().with_outputs_from_drv(&drv, &drv_hash_hex, &HashMap::new());

        assert_eq!(result.status, BuildStatus::Built);
        assert_eq!(result.built_outputs.len(), 1);
        assert_eq!(
            result.built_outputs[0].drv_output_id,
            format!("sha256:{drv_hash_hex}!out")
        );
        assert_eq!(
            result.built_outputs[0].out_path,
            "/nix/store/hf9x46xx06qmkj0ivfqdswgi2qzd2cwz-fixed"
        );
        Ok(())
    }

    #[test]
    fn to_aterm_modulo_no_rewrites_matches_to_aterm() -> anyhow::Result<()> {
        let drv = leaf_ia_drv();
        let modulo = drv.to_aterm_modulo(&BTreeMap::new(), false)?;
        assert_eq!(modulo, drv.to_aterm());
        Ok(())
    }

    #[test]
    fn to_aterm_modulo_missing_key_errors() -> anyhow::Result<()> {
        let drv = Derivation::parse(
            r#"Derive([("out","/nix/store/1a4dmaqd1jgkj2kk6azvzqlvk8qvpq31-test","","")],[("/nix/store/mjwdi20mx2gg223z6jpgpwq7k4wp74zi.drv",["out"])],[],"x86_64-linux","/bin/sh",[],[("name","test"),("system","x86_64-linux")])"#,
        )?;

        let result = drv.to_aterm_modulo(&BTreeMap::new(), false);
        assert!(matches!(result, Err(DerivationError::InputNotFound(_))));
        Ok(())
    }

    /// Helper: a CA-floating leaf (no inputDrvs) with the canonical
    /// hashPlaceholder("out") env value Nix writes for CA outputs.
    fn ca_floating_leaf() -> Derivation {
        Derivation::parse(
            r#"Derive([("out","","r:sha256","")],[],[],"x86_64-linux","/bin/sh",["-c","echo"],[("name","ca-leaf"),("out","/1rz4g4znpzjwh1xymhjpm42vipw92pr73vdgl6xs1hycac8kf2n9"),("system","x86_64-linux")])"#,
        )
        .expect("static fixture")
    }

    /// Nix 2.18-2.20 hashes inputs via `pathDerivationModulo` with
    /// `maskOutputs=false` — only the top-level subject masks. A
    /// CA-floating leaf Y appearing as an input to CA-floating X must be
    /// hashed UNMASKED (env `out` placeholder kept), not masked.
    #[test]
    fn ca_on_ca_input_uses_unmasked_hash() -> anyhow::Result<()> {
        use sha2::{Digest, Sha256};

        let y = ca_floating_leaf();
        // X: CA-floating, depends on Y.
        let x = Derivation::parse(
            r#"Derive([("out","","r:sha256","")],[("/nix/store/f23masp5n2jmrjnxda2x0l9dw45d8lsb-ca-leaf.drv",["out"])],[],"x86_64-linux","/bin/sh",["-c","echo"],[("name","ca-x"),("out","/04fmi8q93y9c8zd2hcq8dckk8lgm75wqjaj4hbn03ikl5ich30bi"),("system","x86_64-linux")])"#,
        )?;
        assert!(x.has_ca_floating_outputs());

        let mut cache = HashMap::new();
        let resolve = |p: &str| -> Option<&Derivation> {
            (p == "/nix/store/f23masp5n2jmrjnxda2x0l9dw45d8lsb-ca-leaf.drv").then_some(&y)
        };
        hash_derivation_modulo(
            &x,
            "/nix/store/dj451jq5mjppa2vgn2g7g40pkw4zgcp9-ca-x.drv",
            &resolve,
            &mut cache,
        )?;

        // Y's UNMASKED hash (mask_outputs=false → env `out` placeholder kept).
        let y_unmasked: [u8; 32] =
            Sha256::digest(y.to_aterm_modulo(&BTreeMap::new(), false)?.as_bytes()).into();
        // Y's MASKED hash (env `out` blanked) — what the buggy code computed.
        let y_masked: [u8; 32] =
            Sha256::digest(y.to_aterm_modulo(&BTreeMap::new(), true)?.as_bytes()).into();
        assert_ne!(y_unmasked, y_masked, "fixture must distinguish mask modes");

        // The cache holds Y's input-form (mask=false) hash.
        assert_eq!(
            cache.get("/nix/store/f23masp5n2jmrjnxda2x0l9dw45d8lsb-ca-leaf.drv"),
            Some(&y_unmasked),
            "CA-floating input must be hashed with mask_outputs=false"
        );
        Ok(())
    }

    /// `hash_cache` stores only the mask=false form. Computing a
    /// CA-floating drv's top-level (mask=true) hash must NOT poison the
    /// cache for later consumers using it as an input.
    #[test]
    fn ca_top_level_not_cached_as_input() -> anyhow::Result<()> {
        use sha2::{Digest, Sha256};

        let y = ca_floating_leaf();
        let y_unmasked: [u8; 32] =
            Sha256::digest(y.to_aterm_modulo(&BTreeMap::new(), false)?.as_bytes()).into();

        let mut cache = HashMap::new();
        let resolve_none = |_: &str| -> Option<&Derivation> { None };
        // Top-level call on Y (mask=true since Y is CA-floating).
        let y_top = hash_derivation_modulo(
            &y,
            "/nix/store/f23masp5n2jmrjnxda2x0l9dw45d8lsb.drv",
            &resolve_none,
            &mut cache,
        )?;
        assert_ne!(y_top, y_unmasked, "top-level CA hash is the masked form");
        // mask=true result must NOT be cached.
        assert!(
            !cache.contains_key("/nix/store/f23masp5n2jmrjnxda2x0l9dw45d8lsb.drv"),
            "mask=true hash must not populate the (mask=false) cache"
        );

        // Now compute a consumer X depending on Y with the SAME cache.
        let x = Derivation::parse(
            r#"Derive([("out","/nix/store/dj451jq5mjppa2vgn2g7g40pkw4zgcp9-ia","","")],[("/nix/store/f23masp5n2jmrjnxda2x0l9dw45d8lsb.drv",["out"])],[],"x86_64-linux","/bin/sh",["-c","echo"],[("name","ia-x"),("out","/nix/store/dj451jq5mjppa2vgn2g7g40pkw4zgcp9-ia"),("system","x86_64-linux")])"#,
        )?;
        let resolve = |p: &str| -> Option<&Derivation> {
            (p == "/nix/store/f23masp5n2jmrjnxda2x0l9dw45d8lsb.drv").then_some(&y)
        };
        hash_derivation_modulo(
            &x,
            "/nix/store/dj451jq5mjppa2vgn2g7g40pkw4zgcp9.drv",
            &resolve,
            &mut cache,
        )?;
        // After recursing, Y's mask=false hash is cached and is the unmasked one.
        assert_eq!(
            cache.get("/nix/store/f23masp5n2jmrjnxda2x0l9dw45d8lsb.drv"),
            Some(&y_unmasked)
        );
        Ok(())
    }

    /// Nix C++ `inputs2[h].insert(outputName)` set-unions when two
    /// inputDrvs collide on modular hash. Last-write-wins drops outputs
    /// → divergent ATerm → divergent consumer hash.
    #[test]
    fn to_aterm_modulo_merges_colliding_rewrite_keys() -> anyhow::Result<()> {
        let drv = Derivation::parse(
            r#"Derive([("out","/nix/store/h215ws5mqjq1pnqd7j0incvdyqk96lhp-consumer","","")],[("/nix/store/n2v52szmyja512fxmaax8lixl4dxh4jb-libA.drv",["out"]),("/nix/store/gjamk2f57j5pqymvqamgxla350szmld1-libB.drv",["dev"])],[],"x86_64-linux","/bin/sh",[],[("name","consumer"),("system","x86_64-linux")])"#,
        )?;

        let h = "d".repeat(64);
        let mut rewrites = BTreeMap::new();
        rewrites.insert(
            "/nix/store/n2v52szmyja512fxmaax8lixl4dxh4jb-libA.drv".to_string(),
            h.clone(),
        );
        rewrites.insert(
            "/nix/store/gjamk2f57j5pqymvqamgxla350szmld1-libB.drv".to_string(),
            h.clone(),
        );

        let result = drv.to_aterm_modulo(&rewrites, false)?;

        // Exactly one inputDrvs entry, with the sorted UNION of output names.
        assert!(
            result.contains(&format!(r#"[("{h}",["dev","out"])]"#)),
            "expected merged sorted-union [dev,out]; got: {result}"
        );
        // Neither single-output form should appear (overwrite would leave one).
        assert!(!result.contains(&format!(r#"("{h}",["dev"])]"#)));
        assert!(!result.contains(&format!(r#"("{h}",["out"])]"#)));
        Ok(())
    }

    /// Build a linear input-addressed chain of `n` derivations
    /// (`chain-0` depends on `chain-1` depends on … `chain-(n-1)`),
    /// returning (root_path, resolver map). Paths use zero-padded
    /// digit-only hash parts so they parse as store paths.
    fn linear_ia_chain(n: usize) -> (String, HashMap<String, Derivation>) {
        let drv_path = |i: usize| format!("/nix/store/{i:0>32}-chain-{i}.drv");
        let out_path = |i: usize| format!("/nix/store/{i:0>32}-chain-{i}");
        let mut map = HashMap::new();
        for i in 0..n {
            let inputs = if i + 1 < n {
                format!(r#"[("{}",["out"])]"#, drv_path(i + 1))
            } else {
                "[]".to_owned()
            };
            let aterm = format!(
                r#"Derive([("out","{out}","","")],{inputs},[],"x86_64-linux","/bin/sh",["-c","echo"],[("out","{out}")])"#,
                out = out_path(i),
            );
            map.insert(drv_path(i), Derivation::parse(&aterm).expect("chain ATerm"));
        }
        (drv_path(0), map)
    }

    /// A 600-deep chain hashes with a COLD cache (no recursion-depth
    /// failure) and the result is identical to the value obtained after
    /// warming the cache bottom-up — the iterative walker is
    /// order-independent.
    #[test]
    fn deep_chain_hashes_with_cold_cache() -> anyhow::Result<()> {
        let n = 600;
        let (root, map) = linear_ia_chain(n);
        let resolve = |p: &str| map.get(p);

        let mut cold_cache = HashMap::new();
        let cold = hash_derivation_modulo(&map[&root], &root, &resolve, &mut cold_cache)?;

        // Warm a fresh cache bottom-up, then hash the root again.
        let mut warm_cache = HashMap::new();
        for i in (0..n).rev() {
            let path = format!("/nix/store/{i:0>32}-chain-{i}.drv");
            hash_derivation_modulo(&map[&path], &path, &resolve, &mut warm_cache)?;
        }
        let warm = hash_derivation_modulo(&map[&root], &root, &resolve, &mut warm_cache)?;

        assert_eq!(cold, warm, "cold-cache and warm-cache hashes must agree");
        Ok(())
    }

    /// Same chain through `input_addressed_output_paths`: the cold-cache
    /// derivation succeeds and matches the bottom-up result.
    #[test]
    fn deep_chain_output_paths_cold_cache() -> anyhow::Result<()> {
        let n = 600;
        let (root, map) = linear_ia_chain(n);
        let resolve = |p: &str| map.get(p);

        let mut cold_cache = HashMap::new();
        let cold = input_addressed_output_paths(&map[&root], &root, &resolve, &mut cold_cache)?;

        let mut warm_cache = HashMap::new();
        for i in (1..n).rev() {
            let path = format!("/nix/store/{i:0>32}-chain-{i}.drv");
            hash_derivation_modulo(&map[&path], &path, &resolve, &mut warm_cache)?;
        }
        let warm = input_addressed_output_paths(&map[&root], &root, &resolve, &mut warm_cache)?;

        assert_eq!(cold["out"], warm["out"]);
        Ok(())
    }

    /// Cache-first input resolution: pre-seeding the hash cache with an
    /// input's modulo hash lets `input_addressed_output_paths` derive the
    /// SAME paths without being able to resolve that input at all. This
    /// is the enabler for computing IA paths from inline content whose
    /// inputs live only as sibling ca_modular_hash declarations.
    #[test]
    fn seeded_cache_replaces_input_resolution() -> anyhow::Result<()> {
        let n = 4;
        let (root, map) = linear_ia_chain(n);

        // Ground truth: full resolution.
        let resolve_all = |p: &str| map.get(p);
        let mut full_cache = HashMap::new();
        let full = input_addressed_output_paths(&map[&root], &root, &resolve_all, &mut full_cache)?;

        // Cache-only: the root's direct input hash is seeded from the
        // full run's cache; the resolver can no longer see ANY input.
        let direct_input = map[&root]
            .input_drvs()
            .keys()
            .next()
            .expect("chain root has one input")
            .clone();
        let seeded_hash = *full_cache
            .get(&direct_input)
            .expect("full run cached the direct input's modulo hash");
        let mut seeded_cache = HashMap::new();
        seeded_cache.insert(direct_input, seeded_hash);
        let resolve_none = |_: &str| -> Option<&Derivation> { None };
        let seeded =
            input_addressed_output_paths(&map[&root], &root, &resolve_none, &mut seeded_cache)?;

        assert_eq!(
            full["out"], seeded["out"],
            "seeded-cache derivation must produce the same paths as full resolution"
        );
        Ok(())
    }

    /// Fail-closed unchanged: with no seed and no resolver, an
    /// unresolvable input is still an InputNotFound error — the cache
    /// fast path must not weaken the error behaviour.
    #[test]
    fn unseeded_unresolvable_input_still_fails() {
        let n = 3;
        let (root, map) = linear_ia_chain(n);
        let resolve_none = |_: &str| -> Option<&Derivation> { None };
        let mut cache = HashMap::new();
        let err = input_addressed_output_paths(&map[&root], &root, &resolve_none, &mut cache)
            .unwrap_err();
        assert!(
            matches!(err, DerivationError::InputNotFound(_)),
            "unseeded unresolvable input fails closed, got: {err:?}"
        );
    }

    /// A 600-node chain whose deepest node references the root is still
    /// reported as a cycle (and terminates) — removing the depth cap
    /// must not turn deep cycles into hangs or stack overflows.
    #[test]
    fn deep_cycle_still_detected() -> anyhow::Result<()> {
        let n = 600;
        let drv_path = |i: usize| format!("/nix/store/{i:0>32}-cycle-{i}.drv");
        let out_path = |i: usize| format!("/nix/store/{i:0>32}-cycle-{i}");
        let mut map = HashMap::new();
        for i in 0..n {
            // Last node points back at the root → cycle.
            let next = if i + 1 < n {
                drv_path(i + 1)
            } else {
                drv_path(0)
            };
            let aterm = format!(
                r#"Derive([("out","{out}","","")],[("{next}",["out"])],[],"x86_64-linux","/bin/sh",["-c","echo"],[("out","{out}")])"#,
                out = out_path(i),
            );
            map.insert(drv_path(i), Derivation::parse(&aterm).expect("chain ATerm"));
        }
        let root = drv_path(0);
        let resolve = |p: &str| map.get(p);
        let mut cache = HashMap::new();
        let err = hash_derivation_modulo(&map[&root], &root, &resolve, &mut cache).unwrap_err();
        assert!(
            matches!(err, DerivationError::CycleDetected(_)),
            "expected CycleDetected, got: {err:?}"
        );
        Ok(())
    }
}
