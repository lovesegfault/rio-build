//! Golden tests: [`input_addressed_output_paths`] must reproduce the exact
//! output paths real Nix computed for real `.drv` files.
//!
//! The fixtures in `tests/fixtures/drv/` were produced by `nix-instantiate`
//! from four tiny `derivation {}` calls: an input-addressed leaf, a
//! multi-output input-addressed derivation, a flat fixed-output derivation,
//! and a consumer whose `inputDrvs` span all three (so the FOD fingerprint
//! arm, the modular-hash recursion, and `outputPathName` naming are all
//! exercised against the oracle). The file names embed the store hashes Nix
//! assigned, so regenerating them after any change is loud in review.
//!
//! These paths are the trust boundary: a gateway/store that recomputes them
//! can reject a derivation claiming somebody else's output path, no matter
//! what an untrusted client or worker declares.

use std::collections::{BTreeMap, HashMap};
use std::fs;
use std::path::PathBuf;

use rio_nix::derivation::{Derivation, DerivationError, input_addressed_output_paths};

fn fixture_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/drv")
}

/// Load every fixture drv keyed by its original store path.
fn load_fixtures() -> BTreeMap<String, Derivation> {
    let mut map = BTreeMap::new();
    for entry in fs::read_dir(fixture_dir()).expect("fixture dir") {
        let entry = entry.expect("dir entry");
        let name = entry.file_name().into_string().expect("utf8 fixture name");
        if !name.ends_with(".drv") {
            continue;
        }
        let text = fs::read_to_string(entry.path()).expect("read fixture");
        // Fixtures carry a trailing newline (end-of-file-fixer); the ATerm is the trimmed line.
        let drv = Derivation::parse(text.trim_end()).expect("parse fixture");
        map.insert(format!("/nix/store/{name}"), drv);
    }
    map
}

fn fixture_path<'a>(fixtures: &'a BTreeMap<String, Derivation>, suffix: &str) -> &'a str {
    fixtures
        .keys()
        .find(|k| k.ends_with(suffix))
        .unwrap_or_else(|| panic!("fixture ending in {suffix} present"))
}

/// Assert that the derived output paths equal the paths Nix declared in the
/// fixture, for every output.
fn assert_matches_oracle(fixtures: &BTreeMap<String, Derivation>, drv_path: &str) {
    let drv = &fixtures[drv_path];
    let resolve = |p: &str| fixtures.get(p);
    let mut cache = HashMap::new();
    let derived = input_addressed_output_paths(drv, drv_path, &resolve, &mut cache)
        .expect("derive output paths");

    assert_eq!(derived.len(), drv.outputs().len());
    for output in drv.outputs() {
        let got = derived
            .get(output.name())
            .unwrap_or_else(|| panic!("derived map has output {}", output.name()));
        assert_eq!(
            got.as_str(),
            output.path(),
            "output `{}` of {drv_path}: derived path must equal the path Nix computed",
            output.name()
        );
    }
}

#[test]
fn golden_ia_leaf_matches_nix() {
    let fixtures = load_fixtures();
    let path = fixture_path(&fixtures, "-rio-golden-leaf.drv").to_owned();
    assert_matches_oracle(&fixtures, &path);
}

#[test]
fn golden_ia_multi_output_matches_nix() {
    let fixtures = load_fixtures();
    let path = fixture_path(&fixtures, "-rio-golden-multi.drv").to_owned();
    assert_matches_oracle(&fixtures, &path);
}

#[test]
fn golden_ia_consumer_with_fod_and_ia_inputs_matches_nix() {
    let fixtures = load_fixtures();
    let path = fixture_path(&fixtures, "-rio-golden-consumer.drv").to_owned();
    assert_matches_oracle(&fixtures, &path);
}

#[test]
fn golden_ia_structured_attrs_matches_nix() {
    let fixtures = load_fixtures();
    let path = fixture_path(&fixtures, "-rio-golden-structured.drv").to_owned();
    assert_matches_oracle(&fixtures, &path);
}

#[test]
fn golden_fod_is_rejected() {
    let fixtures = load_fixtures();
    let path = fixture_path(&fixtures, "-rio-golden-fod.drv").to_owned();
    let drv = &fixtures[&path];
    let resolve = |p: &str| fixtures.get(p);
    let mut cache = HashMap::new();
    let err = input_addressed_output_paths(drv, &path, &resolve, &mut cache)
        .expect_err("fixed-output derivations must not take the IA path rule");
    assert!(matches!(err, DerivationError::NotInputAddressed(_)));
}

#[test]
fn floating_ca_is_rejected() {
    // Synthetic floating-CA shape (hash_algo set, hash empty): no static
    // path exists, so the IA rule must refuse rather than invent one.
    let drv = Derivation::parse(
        r#"Derive([("out","","r:sha256","")],[],[],"x86_64-linux","/bin/sh",["-c","echo"],[("name","ca-float"),("out","/1rz4g4znpzjwh1xymhjpm42vipw92pr73vdgl6xs1hycac8kf2n9"),("system","x86_64-linux")])"#,
    )
    .expect("parse synthetic CA drv");
    let resolve = |_: &str| -> Option<&Derivation> { None };
    let mut cache = HashMap::new();
    let err = input_addressed_output_paths(
        &drv,
        "/nix/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-ca-float.drv",
        &resolve,
        &mut cache,
    )
    .expect_err("floating-CA derivations must be rejected");
    assert!(matches!(err, DerivationError::NotInputAddressed(_)));
}

/// The security property: the declared path has no influence on the derived
/// path. A consumer drv whose declared output path is swapped for somebody
/// else's path still derives to its own original path, so a validator
/// comparing declared vs derived catches the tamper.
#[test]
fn tampered_declared_path_is_detected() {
    let fixtures = load_fixtures();
    let consumer_path = fixture_path(&fixtures, "-rio-golden-consumer.drv").to_owned();
    let leaf_path = fixture_path(&fixtures, "-rio-golden-leaf.drv").to_owned();

    let original_out = fixtures[&consumer_path]
        .outputs()
        .iter()
        .find(|o| o.name() == "out")
        .expect("consumer has out")
        .path()
        .to_owned();
    let victim_out = fixtures[&leaf_path]
        .outputs()
        .iter()
        .find(|o| o.name() == "out")
        .expect("leaf has out")
        .path()
        .to_owned();
    assert_ne!(original_out, victim_out);

    // Re-parse the consumer with its declared output path (and matching env
    // value) replaced by the victim's path — exactly the crafted-drv attack.
    let consumer_text =
        fs::read_to_string(fixture_dir().join(consumer_path.strip_prefix("/nix/store/").unwrap()))
            .expect("read consumer fixture");
    let tampered_text = consumer_text.trim_end().replace(&original_out, &victim_out);
    assert_ne!(consumer_text, tampered_text, "tamper must change the drv");
    let tampered = Derivation::parse(&tampered_text).expect("parse tampered drv");

    let resolve = |p: &str| fixtures.get(p);
    let mut cache = HashMap::new();
    let derived = input_addressed_output_paths(&tampered, &consumer_path, &resolve, &mut cache)
        .expect("derive output paths for tampered drv");

    // The derivation still derives to its own honest path…
    assert_eq!(derived["out"].as_str(), original_out);
    // …which differs from what the tampered drv declares — the validator's
    // comparison catches the spoof.
    assert_ne!(derived["out"].as_str(), tampered.outputs()[0].path());
}
