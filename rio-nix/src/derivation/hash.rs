//! Modular derivation hashing (Nix `hashDerivationModulo`).

use std::collections::{BTreeMap, HashMap, HashSet};

use sha2::{Digest, Sha256};

use super::{Derivation, DerivationError, MAX_HASH_RECURSION_DEPTH};

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

#[cfg(test)]
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
            hash_derivation_modulo(&drv, "/nix/store/xyz-fixed.drv", &resolve, &mut cache).unwrap();

        // Expected: SHA-256("fixed:out:sha256:e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855:")
        let expected: [u8; 32] = Sha256::digest(
            b"fixed:out:sha256:e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855:",
        )
        .into();

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
            hash_derivation_modulo(&drv, "/nix/store/abc-leaf.drv", &resolve, &mut cache).unwrap();

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
        )
        .into();
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

        let hash =
            hash_derivation_modulo(&c, "/nix/store/ghi-chain.drv", &resolve, &mut cache).unwrap();

        // Both A and B should now be cached
        assert!(cache.contains_key("/nix/store/xyz-fixed.drv"));
        assert!(cache.contains_key("/nix/store/def-dependent.drv"));
        assert!(cache.contains_key("/nix/store/ghi-chain.drv"));

        // Verify determinism: computing again gives same result
        let hash2 =
            hash_derivation_modulo(&c, "/nix/store/ghi-chain.drv", &resolve, &mut cache).unwrap();
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
            hash_derivation_modulo(&drv, "/nix/store/abc-leaf.drv", &resolve, &mut cache).unwrap();
        assert_eq!(cache.len(), 1);
        assert!(cache.contains_key("/nix/store/abc-leaf.drv"));

        let hash2 =
            hash_derivation_modulo(&drv, "/nix/store/abc-leaf.drv", &resolve, &mut cache).unwrap();
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
        let hash =
            hash_derivation_modulo(&drv, "/nix/store/xyz-rec.drv", &resolve, &mut cache).unwrap();

        // Expected: SHA-256("fixed:out:r:sha256:e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855:")
        let expected: [u8; 32] = Sha256::digest(
            b"fixed:out:r:sha256:e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855:",
        )
        .into();

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
            hash_derivation_modulo(&d, "/nix/store/ddd-diamond.drv", &resolve, &mut cache).unwrap();

        // All 4 should be cached
        assert_eq!(cache.len(), 4);
        assert!(cache.contains_key("/nix/store/xyz-fixed.drv"));

        // Determinism check
        let hash2 =
            hash_derivation_modulo(&d, "/nix/store/ddd-diamond.drv", &resolve, &mut cache).unwrap();
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

        let result = hash_derivation_modulo(&a, "/nix/store/aaa-cycle.drv", &resolve, &mut cache);
        assert!(matches!(result, Err(DerivationError::CycleDetected(_))));
    }

    #[test]
    fn with_outputs_from_drv_produces_correct_ids() {
        use crate::protocol::build::{BuildResult, BuildStatus};

        let drv = fod_drv(); // FOD with known hash
        let mut cache = HashMap::new();
        let resolve = |_: &str| -> Option<&Derivation> { None };
        let hash =
            hash_derivation_modulo(&drv, "/nix/store/xyz-fixed.drv", &resolve, &mut cache).unwrap();
        let drv_hash_hex = hex::encode(hash);

        let result = BuildResult::success().with_outputs_from_drv(&drv, &drv_hash_hex);

        assert_eq!(result.status, BuildStatus::Built);
        assert_eq!(result.built_outputs.len(), 1);
        assert_eq!(
            result.built_outputs[0].drv_output_id,
            format!("sha256:{drv_hash_hex}!out")
        );
        assert_eq!(result.built_outputs[0].out_path, "/nix/store/xyz-fixed");
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
