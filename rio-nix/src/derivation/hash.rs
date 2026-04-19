//! Modular derivation hashing (Nix `hashDerivationModulo`).

use std::collections::{BTreeMap, HashMap, HashSet};

use sha2::{Digest, Sha256};

use super::{Derivation, DerivationError, DerivationLike, MAX_HASH_RECURSION_DEPTH};

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
/// `resolve_input` maps a drv path string (e.g. `"/nix/store/abc.drv"`) to
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
    let mut visiting = HashSet::new();
    // Nix 2.18-2.20: top-level entry (`staticOutputHashes`) passes
    // maskOutputs=true for CA-floating; recursive entry
    // (`pathDerivationModulo`) hard-codes false. We mirror that — the
    // top-level call masks iff this drv is CA-floating; the recursive
    // call below always passes false.
    let mask_outputs = drv.has_ca_floating_outputs();
    hash_derivation_modulo_inner(
        drv,
        drv_path,
        resolve_input,
        hash_cache,
        &mut visiting,
        0,
        mask_outputs,
    )
}

/// `mask_outputs` is threaded explicitly (NOT recomputed per level): a
/// CA-floating drv has *two* distinct modular hashes — `mask=true` when it
/// is the realisation-key subject, `mask=false` when it appears as an
/// input to another drv. `hash_cache` stores only the `mask=false` value
/// (the form reused across input lookups), mirroring Nix where `drvHashes`
/// lives in `pathDerivationModulo`, not `hashDerivationModulo`.
fn hash_derivation_modulo_inner<'c>(
    drv: &'c Derivation,
    drv_path: &str,
    resolve_input: &dyn Fn(&str) -> Option<&'c Derivation>,
    hash_cache: &mut HashMap<String, [u8; 32]>,
    visiting: &mut HashSet<String>,
    depth: usize,
    mask_outputs: bool,
) -> Result<[u8; 32], DerivationError> {
    // Memoisation — cache holds the mask=false form only.
    if !mask_outputs && let Some(&cached) = hash_cache.get(drv_path) {
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
            // Nix C++ derivations.cc:hashDerivationModulo — the output path
            // IS part of the fingerprint. The trailing-colon-no-path shape
            // was a copy-paste from store_path.rs make_store_path_hash
            // where it IS correct (different function). See phase4a.md §5.
            let fingerprint = format!(
                "fixed:out:{}:{}:{}",
                output.hash_algo(),
                output.hash(),
                output.path()
            );
            Ok(Sha256::digest(fingerprint.as_bytes()).into())
        } else {
            // Input-addressed or CA floating: recurse on input drvs.
            // Inputs are ALWAYS hashed with mask_outputs=false (Nix
            // `pathDerivationModulo`), regardless of the input's own
            // CA-floating-ness — only the top-level subject masks.
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
                    false,
                )?;
                input_rewrites.insert(input_drv_path.clone(), hex::encode(input_hash));
            }

            let modified_aterm = drv.to_aterm_modulo(&input_rewrites, mask_outputs)?;
            Ok(Sha256::digest(modified_aterm.as_bytes()).into())
        }
    })();

    visiting.remove(drv_path);

    let hash = result?;
    if !mask_outputs {
        hash_cache.insert(drv_path.to_string(), hash);
    }
    Ok(hash)
}

#[cfg(test)]
mod hash_derivation_modulo_tests {
    use super::*;
    use std::collections::{BTreeMap, HashMap};

    /// Helper: create a simple input-addressed derivation (no inputDrvs).
    fn leaf_ia_drv() -> Derivation {
        Derivation::parse(
                r#"Derive([("out","/nix/store/abc-leaf","","")],[],[],"x86_64-linux","/bin/sh",["-c","echo hello"],[("name","leaf"),("out","/nix/store/abc-leaf"),("system","x86_64-linux")])"#,
            ).expect("static fixture")
    }

    /// Helper: create a fixed-output derivation.
    fn fod_drv() -> Derivation {
        Derivation::parse(
                r#"Derive([("out","/nix/store/xyz-fixed","sha256","e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855")],[],[],"x86_64-linux","/bin/sh",["-c","echo"],[("name","fixed"),("out","/nix/store/xyz-fixed"),("outputHash","e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"),("outputHashAlgo","sha256"),("system","x86_64-linux")])"#,
            ).expect("static fixture")
    }

    /// Helper: create an IA derivation that depends on the FOD.
    fn ia_with_fod_input() -> Derivation {
        Derivation::parse(
                r#"Derive([("out","/nix/store/def-dependent","","")],[("/nix/store/xyz-fixed.drv",["out"])],[],"x86_64-linux","/bin/sh",["-c","echo"],[("name","dependent"),("out","/nix/store/def-dependent"),("system","x86_64-linux")])"#,
            ).expect("static fixture")
    }

    #[test]
    fn fod_hash_matches_fingerprint() -> anyhow::Result<()> {
        use sha2::{Digest, Sha256};

        let drv = fod_drv();
        assert!(drv.is_fixed_output());

        let mut cache = HashMap::new();
        let resolve = |_: &str| -> Option<&Derivation> { None };
        let hash = hash_derivation_modulo(&drv, "/nix/store/xyz-fixed.drv", &resolve, &mut cache)?;

        // Expected: SHA-256("fixed:out:sha256:<hex>:/nix/store/xyz-fixed")
        let expected: [u8; 32] = Sha256::digest(
            b"fixed:out:sha256:e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855:/nix/store/xyz-fixed",
        )
        .into();

        assert_eq!(hash, expected);
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
        let hash = hash_derivation_modulo(&drv, "/nix/store/abc-leaf.drv", &resolve, &mut cache)?;

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
            if path == "/nix/store/xyz-fixed.drv" {
                Some(&fod)
            } else {
                None
            }
        };
        let hash =
            hash_derivation_modulo(&dep, "/nix/store/def-dependent.drv", &resolve, &mut cache)?;

        // The FOD modular hash
        let fod_hash: [u8; 32] = Sha256::digest(
            b"fixed:out:sha256:e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855:/nix/store/xyz-fixed",
        )
        .into();
        let fod_hex = hex::encode(fod_hash);

        // The modified ATerm should have the FOD hex hash instead of the drv path
        let mut rewrites = BTreeMap::new();
        rewrites.insert("/nix/store/xyz-fixed.drv".to_string(), fod_hex.clone());
        let modified_aterm = dep.to_aterm_modulo(&rewrites, false)?;

        // Verify the modified ATerm contains the hex hash, not the drv path
        assert!(modified_aterm.contains(&fod_hex));
        assert!(!modified_aterm.contains("/nix/store/xyz-fixed.drv"));

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
            r#"Derive([("out","/nix/store/ghi-chain","","")],[("/nix/store/def-dependent.drv",["out"])],[],"x86_64-linux","/bin/sh",["-c","echo"],[("name","chain"),("out","/nix/store/ghi-chain"),("system","x86_64-linux")])"#,
        )?;

        let mut cache = HashMap::new();
        let resolve = |path: &str| -> Option<&Derivation> {
            match path {
                "/nix/store/xyz-fixed.drv" => Some(&fod_a),
                "/nix/store/def-dependent.drv" => Some(&b),
                _ => None,
            }
        };

        let hash = hash_derivation_modulo(&c, "/nix/store/ghi-chain.drv", &resolve, &mut cache)?;

        // Both A and B should now be cached
        assert!(cache.contains_key("/nix/store/xyz-fixed.drv"));
        assert!(cache.contains_key("/nix/store/def-dependent.drv"));
        assert!(cache.contains_key("/nix/store/ghi-chain.drv"));

        // Verify determinism: computing again gives same result
        let hash2 = hash_derivation_modulo(&c, "/nix/store/ghi-chain.drv", &resolve, &mut cache)?;
        assert_eq!(hash, hash2);
        Ok(())
    }

    #[test]
    fn multi_output_with_outputs_from_drv() -> anyhow::Result<()> {
        use crate::protocol::build::{BuildResult, BuildStatus};

        let drv = Derivation::parse(
            r#"Derive([("dev","/nix/store/abc-dev","",""),("lib","/nix/store/abc-lib","",""),("out","/nix/store/abc-out","","")],[],[],"x86_64-linux","/bin/sh",["-c","echo"],[("name","multi"),("out","/nix/store/abc-out"),("system","x86_64-linux")])"#,
        )?;

        let mut cache = HashMap::new();
        let resolve = |_: &str| -> Option<&Derivation> { None };
        let hash = hash_derivation_modulo(&drv, "/nix/store/abc.drv", &resolve, &mut cache)?;
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
        assert_eq!(result.built_outputs[0].out_path, "/nix/store/abc-dev");

        assert_eq!(
            result.built_outputs[1].drv_output_id,
            format!("sha256:{drv_hash_hex}!lib")
        );
        assert_eq!(result.built_outputs[1].out_path, "/nix/store/abc-lib");

        assert_eq!(
            result.built_outputs[2].drv_output_id,
            format!("sha256:{drv_hash_hex}!out")
        );
        assert_eq!(result.built_outputs[2].out_path, "/nix/store/abc-out");
        Ok(())
    }

    #[test]
    fn ca_floating_masks_output_paths() -> anyhow::Result<()> {
        // CA floating: hash_algo set, hash empty
        let ca_drv = Derivation::parse(
            r#"Derive([("out","/nix/store/placeholder-ca","sha256","")],[],[],"x86_64-linux","/bin/sh",["-c","echo"],[("name","ca-test"),("out","/nix/store/placeholder-ca"),("system","x86_64-linux")])"#,
        )?;

        assert!(ca_drv.has_ca_floating_outputs());
        assert!(!ca_drv.is_fixed_output()); // hash is empty, so not FOD

        let mut cache = HashMap::new();
        let resolve = |_: &str| -> Option<&Derivation> { None };
        let hash = hash_derivation_modulo(&ca_drv, "/nix/store/ca.drv", &resolve, &mut cache)?;

        // Verify the hash uses masked ATerm (empty output path AND empty
        // env value for the `out` key — Nix C++ masks both; missing the
        // env mask produces a hash that nix-build's wopQueryRealisation
        // will never match).
        use sha2::{Digest, Sha256};
        let masked_aterm = ca_drv.to_aterm_modulo(&BTreeMap::new(), true)?;
        assert!(masked_aterm.contains(r#"("out","","sha256","")"#));
        // Env `out` value masked: original fixture has
        // ("out","/nix/store/placeholder-ca"), masked has ("out","").
        assert!(
            masked_aterm.contains(r#"("out","")"#),
            "env out var should be masked to empty; got: {masked_aterm}"
        );
        assert!(
            !masked_aterm.contains("/nix/store/placeholder-ca"),
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

        let hash1 = hash_derivation_modulo(&drv, "/nix/store/abc-leaf.drv", &resolve, &mut cache)?;
        assert_eq!(cache.len(), 1);
        assert!(cache.contains_key("/nix/store/abc-leaf.drv"));

        let hash2 = hash_derivation_modulo(&drv, "/nix/store/abc-leaf.drv", &resolve, &mut cache)?;
        assert_eq!(hash1, hash2);
        Ok(())
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
    fn cycle_detection() -> anyhow::Result<()> {
        // Create a derivation that references itself
        let cyclic = Derivation::parse(
            r#"Derive([("out","/nix/store/abc-cyclic","","")],[("/nix/store/abc-cyclic.drv",["out"])],[],"x86_64-linux","/bin/sh",["-c","echo"],[("name","cyclic"),("out","/nix/store/abc-cyclic"),("system","x86_64-linux")])"#,
        )?;

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
        Ok(())
    }

    #[test]
    fn to_aterm_modulo_replaces_input_keys() -> anyhow::Result<()> {
        let drv = Derivation::parse(
            r#"Derive([("out","/nix/store/abc-test","","")],[("/nix/store/aaa-input.drv",["out"]),("/nix/store/bbb-input.drv",["dev","out"])],[],"x86_64-linux","/bin/sh",[],[("name","test"),("system","x86_64-linux")])"#,
        )?;

        let mut rewrites = BTreeMap::new();
        rewrites.insert(
            "/nix/store/aaa-input.drv".to_string(),
            "aaaa".repeat(16), // 64-char hex
        );
        rewrites.insert("/nix/store/bbb-input.drv".to_string(), "bbbb".repeat(16));

        let result = drv.to_aterm_modulo(&rewrites, false)?;

        // Keys should be the hex hashes, not the drv paths
        assert!(result.contains(&"aaaa".repeat(16)));
        assert!(result.contains(&"bbbb".repeat(16)));
        assert!(!result.contains("/nix/store/aaa-input.drv"));
        assert!(!result.contains("/nix/store/bbb-input.drv"));

        // Output paths should be preserved (mask_outputs=false)
        assert!(result.contains("/nix/store/abc-test"));
        Ok(())
    }

    #[test]
    fn to_aterm_modulo_sorts_by_replacement_keys() -> anyhow::Result<()> {
        // Original keys sorted: aaa < bbb
        // Replacement keys sorted: zzzz > aaaa (reversed!)
        let drv = Derivation::parse(
            r#"Derive([("out","/nix/store/abc-test","","")],[("/nix/store/aaa.drv",["out"]),("/nix/store/bbb.drv",["out"])],[],"x86_64-linux","/bin/sh",[],[("name","test"),("system","x86_64-linux")])"#,
        )?;

        let mut rewrites = BTreeMap::new();
        rewrites.insert(
            "/nix/store/aaa.drv".to_string(),
            "z".repeat(64), // sorts AFTER b...
        );
        rewrites.insert(
            "/nix/store/bbb.drv".to_string(),
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
            r#"Derive([("out","/nix/store/xyz-rec","r:sha256","e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855")],[],[],"x86_64-linux","/bin/sh",["-c","echo"],[("name","rec"),("out","/nix/store/xyz-rec"),("outputHash","e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"),("outputHashAlgo","r:sha256"),("system","x86_64-linux")])"#,
        )?;

        assert!(drv.is_fixed_output());

        let mut cache = HashMap::new();
        let resolve = |_: &str| -> Option<&Derivation> { None };
        let hash = hash_derivation_modulo(&drv, "/nix/store/xyz-rec.drv", &resolve, &mut cache)?;

        // Expected: SHA-256("fixed:out:r:sha256:<hex>:/nix/store/xyz-rec")
        let expected: [u8; 32] = Sha256::digest(
            b"fixed:out:r:sha256:e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855:/nix/store/xyz-rec",
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
            r#"Derive([("out","/nix/store/aaa-fixed","sha256","e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855")],[],[],"x86_64-linux","/bin/sh",["-c","echo"],[("name","fixed-a"),("out","/nix/store/aaa-fixed"),("outputHash","e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"),("outputHashAlgo","sha256"),("system","x86_64-linux")])"#,
        )?;
        let drv_b = Derivation::parse(
            r#"Derive([("out","/nix/store/bbb-fixed","sha256","e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855")],[],[],"x86_64-linux","/bin/sh",["-c","echo"],[("name","fixed-b"),("out","/nix/store/bbb-fixed"),("outputHash","e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"),("outputHashAlgo","sha256"),("system","x86_64-linux")])"#,
        )?;

        let mut cache = HashMap::new();
        let resolve = |_: &str| -> Option<&Derivation> { None };
        let hash_a =
            hash_derivation_modulo(&drv_a, "/nix/store/aaa-fixed.drv", &resolve, &mut cache)?;
        let hash_b =
            hash_derivation_modulo(&drv_b, "/nix/store/bbb-fixed.drv", &resolve, &mut cache)?;

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
        let b = ia_with_fod_input(); // depends on /nix/store/xyz-fixed.drv
        let c = Derivation::parse(
            r#"Derive([("out","/nix/store/ccc-other","","")],[("/nix/store/xyz-fixed.drv",["out"])],[],"x86_64-linux","/bin/sh",["-c","echo"],[("name","other"),("out","/nix/store/ccc-other"),("system","x86_64-linux")])"#,
        )?;
        let d = Derivation::parse(
            r#"Derive([("out","/nix/store/ddd-diamond","","")],[("/nix/store/ccc-other.drv",["out"]),("/nix/store/def-dependent.drv",["out"])],[],"x86_64-linux","/bin/sh",["-c","echo"],[("name","diamond"),("out","/nix/store/ddd-diamond"),("system","x86_64-linux")])"#,
        )?;

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
        let hash = hash_derivation_modulo(&d, "/nix/store/ddd-diamond.drv", &resolve, &mut cache)?;

        // All 4 should be cached
        assert_eq!(cache.len(), 4);
        assert!(cache.contains_key("/nix/store/xyz-fixed.drv"));

        // Determinism check
        let hash2 = hash_derivation_modulo(&d, "/nix/store/ddd-diamond.drv", &resolve, &mut cache)?;
        assert_eq!(hash, hash2);
        Ok(())
    }

    #[test]
    fn indirect_cycle_detection() -> anyhow::Result<()> {
        // A -> B -> A (indirect cycle)
        let a = Derivation::parse(
            r#"Derive([("out","/nix/store/aaa-cycle","","")],[("/nix/store/bbb-cycle.drv",["out"])],[],"x86_64-linux","/bin/sh",["-c","echo"],[("name","a"),("system","x86_64-linux")])"#,
        )?;
        let b = Derivation::parse(
            r#"Derive([("out","/nix/store/bbb-cycle","","")],[("/nix/store/aaa-cycle.drv",["out"])],[],"x86_64-linux","/bin/sh",["-c","echo"],[("name","b"),("system","x86_64-linux")])"#,
        )?;

        let mut cache = HashMap::new();
        let resolve = |path: &str| -> Option<&Derivation> {
            match path {
                "/nix/store/aaa-cycle.drv" => Some(&a),
                "/nix/store/bbb-cycle.drv" => Some(&b),
                _ => None,
            }
        };

        let result = hash_derivation_modulo(&a, "/nix/store/aaa-cycle.drv", &resolve, &mut cache);
        assert!(matches!(result, Err(DerivationError::CycleDetected(_))));
        Ok(())
    }

    #[test]
    fn with_outputs_from_drv_produces_correct_ids() -> anyhow::Result<()> {
        use crate::protocol::build::{BuildResult, BuildStatus};

        let drv = fod_drv(); // FOD with known hash
        let mut cache = HashMap::new();
        let resolve = |_: &str| -> Option<&Derivation> { None };
        let hash = hash_derivation_modulo(&drv, "/nix/store/xyz-fixed.drv", &resolve, &mut cache)?;
        let drv_hash_hex = hex::encode(hash);

        let result =
            BuildResult::success().with_outputs_from_drv(&drv, &drv_hash_hex, &HashMap::new());

        assert_eq!(result.status, BuildStatus::Built);
        assert_eq!(result.built_outputs.len(), 1);
        assert_eq!(
            result.built_outputs[0].drv_output_id,
            format!("sha256:{drv_hash_hex}!out")
        );
        assert_eq!(result.built_outputs[0].out_path, "/nix/store/xyz-fixed");
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
            r#"Derive([("out","/nix/store/abc-test","","")],[("/nix/store/missing.drv",["out"])],[],"x86_64-linux","/bin/sh",[],[("name","test"),("system","x86_64-linux")])"#,
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
            r#"Derive([("out","","r:sha256","")],[("/nix/store/yyy-ca-leaf.drv",["out"])],[],"x86_64-linux","/bin/sh",["-c","echo"],[("name","ca-x"),("out","/04fmi8q93y9c8zd2hcq8dckk8lgm75wqjaj4hbn03ikl5ich30bi"),("system","x86_64-linux")])"#,
        )?;
        assert!(x.has_ca_floating_outputs());

        let mut cache = HashMap::new();
        let resolve =
            |p: &str| -> Option<&Derivation> { (p == "/nix/store/yyy-ca-leaf.drv").then_some(&y) };
        hash_derivation_modulo(&x, "/nix/store/xxx-ca-x.drv", &resolve, &mut cache)?;

        // Y's UNMASKED hash (mask_outputs=false → env `out` placeholder kept).
        let y_unmasked: [u8; 32] =
            Sha256::digest(y.to_aterm_modulo(&BTreeMap::new(), false)?.as_bytes()).into();
        // Y's MASKED hash (env `out` blanked) — what the buggy code computed.
        let y_masked: [u8; 32] =
            Sha256::digest(y.to_aterm_modulo(&BTreeMap::new(), true)?.as_bytes()).into();
        assert_ne!(y_unmasked, y_masked, "fixture must distinguish mask modes");

        // The cache holds Y's input-form (mask=false) hash.
        assert_eq!(
            cache.get("/nix/store/yyy-ca-leaf.drv"),
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
        let y_top = hash_derivation_modulo(&y, "/nix/store/yyy.drv", &resolve_none, &mut cache)?;
        assert_ne!(y_top, y_unmasked, "top-level CA hash is the masked form");
        // mask=true result must NOT be cached.
        assert!(
            !cache.contains_key("/nix/store/yyy.drv"),
            "mask=true hash must not populate the (mask=false) cache"
        );

        // Now compute a consumer X depending on Y with the SAME cache.
        let x = Derivation::parse(
            r#"Derive([("out","/nix/store/xxx-ia","","")],[("/nix/store/yyy.drv",["out"])],[],"x86_64-linux","/bin/sh",["-c","echo"],[("name","ia-x"),("out","/nix/store/xxx-ia"),("system","x86_64-linux")])"#,
        )?;
        let resolve =
            |p: &str| -> Option<&Derivation> { (p == "/nix/store/yyy.drv").then_some(&y) };
        hash_derivation_modulo(&x, "/nix/store/xxx.drv", &resolve, &mut cache)?;
        // After recursing, Y's mask=false hash is cached and is the unmasked one.
        assert_eq!(cache.get("/nix/store/yyy.drv"), Some(&y_unmasked));
        Ok(())
    }

    /// Nix C++ `inputs2[h].insert(outputName)` set-unions when two
    /// inputDrvs collide on modular hash. Last-write-wins drops outputs
    /// → divergent ATerm → divergent consumer hash.
    #[test]
    fn to_aterm_modulo_merges_colliding_rewrite_keys() -> anyhow::Result<()> {
        let drv = Derivation::parse(
            r#"Derive([("out","/nix/store/ccc-consumer","","")],[("/nix/store/aaa-libA.drv",["out"]),("/nix/store/bbb-libB.drv",["dev"])],[],"x86_64-linux","/bin/sh",[],[("name","consumer"),("system","x86_64-linux")])"#,
        )?;

        let h = "d".repeat(64);
        let mut rewrites = BTreeMap::new();
        rewrites.insert("/nix/store/aaa-libA.drv".to_string(), h.clone());
        rewrites.insert("/nix/store/bbb-libB.drv".to_string(), h.clone());

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
}
