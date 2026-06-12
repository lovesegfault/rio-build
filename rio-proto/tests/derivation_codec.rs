//! Golden + cross-check tests for the canonical `rio.drv.v1.Derivation`
//! codec (ADR-024).
//!
//! The golden tests pin the exact canonical bytes and blake3 digest of
//! two fixture derivations — a structuredAttrs (`__json`) one and a
//! non-ASCII one. Any change to the encode rule (field order, sort
//! order, default omission) re-keys every stored drv digest, so it must
//! show up here as a hard diff, exactly like the castore
//! `golden_directory_encoding` precedent.

use prost::Message;

use rio_nix::derivation::Derivation as NixDerivation;
use rio_nix::hash::{HashAlgo, NixHash};
use rio_nix::store_path::StorePath;
use rio_proto::derivation_util::{
    DrvBlobError, canonical_encode, derivation_digest, from_proto, to_proto, validate_derivation,
    verify_drv_blob,
};

/// structuredAttrs fixture: multi-output, one input drv with a
/// two-output selection, one input src, `__json` env blob carried
/// opaquely. Store-path hash parts are valid nixbase32.
const STRUCTURED_ATERM: &str = concat!(
    r#"Derive([("dev","/nix/store/1123456789abcdfg0123456789abcdfg-fixture-1.0-dev","",""),"#,
    r#"("out","/nix/store/2123456789abcdfg0123456789abcdfg-fixture-1.0","","")],"#,
    r#"[("/nix/store/3123456789abcdfg0123456789abcdfg-dep-0.1.drv",["dev","out"])],"#,
    r#"["/nix/store/4123456789abcdfg0123456789abcdfg-builder.sh"],"#,
    r#""x86_64-linux","/nix/store/5123456789abcdfg0123456789abcdfg-bash/bin/bash","#,
    r#"["-e",".attrs.sh"],"#,
    r#"[("__json","{\"builder\":\"bash\",\"name\":\"fixture\",\"outputs\":[\"dev\",\"out\"]}"),"#,
    r#"("dev","/nix/store/1123456789abcdfg0123456789abcdfg-fixture-1.0-dev"),"#,
    r#"("out","/nix/store/2123456789abcdfg0123456789abcdfg-fixture-1.0")])"#
);

/// Non-ASCII (UTF-8) fixture: multi-byte env value and arg, plus the
/// ATerm escape set (`\\ \" \n \r \t`) to pin writer behaviour.
const NON_ASCII_ATERM: &str = concat!(
    r#"Derive([("out","/nix/store/6123456789abcdfg0123456789abcdfg-unicode-0.1","","")],"#,
    r#"[],[],"x86_64-linux","/bin/sh","#,
    r#"["-c","echo λx→x ✓ 日本語"],"#,
    "[(\"desc\",\"naïve \\\"quoted\\\" \\\\back\\\\ tab\\there nl\\nend\"),",
    r#"("name","unicode")])"#
);

/// Pinned canonical proto bytes (hex) for [`STRUCTURED_ATERM`].
/// Regenerate ONLY for a deliberate, digest-space-breaking schema
/// change (run with `DUMP_GOLDEN=1 -- --nocapture` to print live
/// values).
const STRUCTURED_CANONICAL_HEX: &str = concat!(
    "0a420a03646576123b2f6e69782f73746f72652f313132333435363738396162",
    "63646667303132333435363738396162636466672d666978747572652d312e30",
    "2d6465760a3e0a036f757412372f6e69782f73746f72652f3231323334353637",
    "3839616263646667303132333435363738396162636466672d66697874757265",
    "2d312e3012430a372f6e69782f73746f72652f33313233343536373839616263",
    "646667303132333435363738396162636466672d6465702d302e312e64727612",
    "0364657612036f75741a362f6e69782f73746f72652f34313233343536373839",
    "616263646667303132333435363738396162636466672d6275696c6465722e73",
    "68220c7838365f36342d6c696e75782a392f6e69782f73746f72652f35313233",
    "343536373839616263646667303132333435363738396162636466672d626173",
    "682f62696e2f6261736832022d6532092e61747472732e73683a450a065f5f6a",
    "736f6e123b7b226275696c646572223a2262617368222c226e616d65223a2266",
    "697874757265222c226f757470757473223a5b22646576222c226f7574225d7d",
    "3a420a03646576123b2f6e69782f73746f72652f313132333435363738396162",
    "63646667303132333435363738396162636466672d666978747572652d312e30",
    "2d6465763a3e0a036f757412372f6e69782f73746f72652f3231323334353637",
    "3839616263646667303132333435363738396162636466672d66697874757265",
    "2d312e30",
);
/// blake3 of the canonical bytes above.
const STRUCTURED_DIGEST_HEX: &str =
    "3ab8bfa09a4bf5e089b5dbff65d30adbbc45110f807d12bc91359ab2e941845f";

/// Pinned canonical proto bytes (hex) for [`NON_ASCII_ATERM`].
const NON_ASCII_CANONICAL_HEX: &str = concat!(
    "0a3e0a036f757412372f6e69782f73746f72652f363132333435363738396162",
    "63646667303132333435363738396162636466672d756e69636f64652d302e31",
    "220c7838365f36342d6c696e75782a072f62696e2f736832022d63321a656368",
    "6f20cebb78e286927820e29c9320e697a5e69cace8aa9e3a2e0a046465736312",
    "266e61c3af7665202271756f74656422205c6261636b5c207461620968657265",
    "206e6c0a656e643a0f0a046e616d651207756e69636f6465",
);
/// blake3 of the canonical bytes above.
const NON_ASCII_DIGEST_HEX: &str =
    "ced8aa47659d703f956bf0e17fe69f95f865e475ae43e5ae54ea6e61dfbb3094";

/// The drv path Nix would mint for this content:
/// `make_text(name, sha256(aterm), input_drvs ∪ input_srcs)`.
fn mint_drv_path(drv: &NixDerivation, aterm: &str, name: &str) -> String {
    let refs: Vec<StorePath> = drv
        .input_drvs()
        .keys()
        .chain(drv.input_srcs().iter())
        .map(|r| StorePath::parse(r).expect("fixture refs parse"))
        .collect();
    let h = NixHash::compute(HashAlgo::SHA256, aterm.as_bytes());
    StorePath::make_text(name, &h, &refs)
        .expect("make_text")
        .to_string()
}

fn golden_case(aterm: &str, canonical_hex: &str, digest_hex: &str, drv_file_name: &str) {
    let drv = NixDerivation::parse(aterm).expect("fixture parses");
    let p = to_proto(&drv);
    validate_derivation(&p).expect("to_proto output is canonical");
    let enc = canonical_encode(&p);
    let digest = derivation_digest(&p);

    if std::env::var_os("DUMP_GOLDEN").is_some() {
        println!("canonical: {}", hex::encode(&enc));
        println!("digest:    {}", hex::encode(digest));
    }
    assert_eq!(hex::encode(&enc), canonical_hex, "canonical bytes moved");
    assert_eq!(hex::encode(digest), digest_hex, "digest moved");

    // Full gateway cross-check on the golden blob.
    let drv_path = mint_drv_path(&drv, aterm, drv_file_name);
    let v = verify_drv_blob(&enc, &digest, &drv_path).expect("verify");
    assert_eq!(v.aterm, aterm, "ATerm round-trip must be byte-identical");
    assert_eq!(v.drv_path.as_str(), drv_path);
    assert_eq!(v.digest, digest);
    assert_eq!(v.derivation, drv);
}

#[test]
fn golden_structured_attrs() {
    golden_case(
        STRUCTURED_ATERM,
        STRUCTURED_CANONICAL_HEX,
        STRUCTURED_DIGEST_HEX,
        "fixture-1.0.drv",
    );
}

#[test]
fn golden_non_ascii() {
    golden_case(
        NON_ASCII_ATERM,
        NON_ASCII_CANONICAL_HEX,
        NON_ASCII_DIGEST_HEX,
        "unicode-0.1.drv",
    );
}

#[test]
fn from_proto_inverts_to_proto() {
    for aterm in [STRUCTURED_ATERM, NON_ASCII_ATERM] {
        let drv = NixDerivation::parse(aterm).unwrap();
        let p = to_proto(&drv);
        let back = from_proto(&p).expect("from_proto");
        assert_eq!(back, drv);
        assert_eq!(back.to_aterm(), aterm);
    }
}

// ---------------------------------------------------------------------------
// Hostile-input rejection: every tamper class must fail verify, never
// be silently repaired.
// ---------------------------------------------------------------------------

fn fixture_blob() -> (Vec<u8>, [u8; 32], String) {
    let drv = NixDerivation::parse(STRUCTURED_ATERM).unwrap();
    let p = to_proto(&drv);
    let enc = canonical_encode(&p);
    let digest = derivation_digest(&p);
    let path = mint_drv_path(&drv, STRUCTURED_ATERM, "fixture-1.0.drv");
    (enc, digest, path)
}

#[test]
fn verify_rejects_digest_mismatch() {
    let (enc, _, path) = fixture_blob();
    let wrong = [0u8; 32];
    assert!(matches!(
        verify_drv_blob(&enc, &wrong, &path),
        Err(DrvBlobError::DigestMismatch { .. })
    ));
}

#[test]
fn verify_rejects_non_canonical_unknown_field() {
    // Valid message + a trailing unknown field (tag 15, varint 0):
    // decodes fine, validates fine, but the canonical re-encode drops
    // the unknown field — byte-compare must reject. Accepting it would
    // give one drv_path a second digest.
    let (mut enc, _, path) = fixture_blob();
    enc.extend_from_slice(&[0x78, 0x00]);
    let digest = *blake3::hash(&enc).as_bytes();
    assert!(matches!(
        verify_drv_blob(&enc, &digest, &path),
        Err(DrvBlobError::NonCanonical)
    ));
}

#[test]
fn verify_rejects_unsorted_env_not_resorts() {
    // Re-keying attack: same logical drv, env pair order swapped.
    // Must fail validation — re-sorting would re-key the digest.
    let drv = NixDerivation::parse(STRUCTURED_ATERM).unwrap();
    let mut p = to_proto(&drv);
    p.env.swap(0, 1);
    let enc = p.encode_to_vec();
    let digest = *blake3::hash(&enc).as_bytes();
    let (_, _, path) = fixture_blob();
    assert!(matches!(
        verify_drv_blob(&enc, &digest, &path),
        Err(DrvBlobError::Invalid(_))
    ));
}

#[test]
fn verify_rejects_drv_path_mismatch() {
    // Content tampered after digest minting: env value flipped, digest
    // honestly recomputed over the tampered blob — only the drv_path
    // recompute (Nix's own text-path anchor) catches it.
    let drv = NixDerivation::parse(STRUCTURED_ATERM).unwrap();
    let mut p = to_proto(&drv);
    p.env[0].value = b"{\"tampered\":true}".to_vec();
    let enc = p.encode_to_vec();
    let digest = *blake3::hash(&enc).as_bytes();
    let (_, _, path) = fixture_blob();
    assert!(matches!(
        verify_drv_blob(&enc, &digest, &path),
        Err(DrvBlobError::DrvPathMismatch { .. })
    ));
}

#[test]
fn verify_rejects_non_utf8_bytes() {
    // bytes-typed schema admits non-UTF-8; rio-nix's model does not.
    // The converter must hard-error (never lossy-replace).
    let drv = NixDerivation::parse(STRUCTURED_ATERM).unwrap();
    let mut p = to_proto(&drv);
    p.env[0].value = vec![0xff, 0xfe];
    let enc = p.encode_to_vec();
    let digest = *blake3::hash(&enc).as_bytes();
    let (_, _, path) = fixture_blob();
    assert!(matches!(
        verify_drv_blob(&enc, &digest, &path),
        Err(DrvBlobError::NonUtf8 { .. })
    ));
}

#[test]
fn verify_checks_fixed_output_path() {
    // FOD with a deliberately wrong recorded output path: the static
    // make_fixed_output recompute must reject. (The drv_path check
    // alone wouldn't — the path IS derived from the lying content.)
    let aterm = concat!(
        r#"Derive([("out","/nix/store/7123456789abcdfg0123456789abcdfg-src.tar.gz","sha256","#,
        r#""0000000000000000000000000000000000000000000000000000000000000000")],[],[],"#,
        r#""builtin","builtin:fetchurl",[],[("out","/nix/store/7123456789abcdfg0123456789abcdfg-src.tar.gz")])"#
    );
    let drv = NixDerivation::parse(aterm).unwrap();
    let p = to_proto(&drv);
    let enc = canonical_encode(&p);
    let digest = derivation_digest(&p);
    let path = mint_drv_path(&drv, aterm, "src.tar.gz.drv");
    assert!(matches!(
        verify_drv_blob(&enc, &digest, &path),
        Err(DrvBlobError::FodPathMismatch { .. })
    ));

    // Same drv with the CORRECT recorded output path passes.
    let zero_hash = "0".repeat(64);
    let nh = NixHash::new(HashAlgo::SHA256, vec![0u8; 32]).unwrap();
    let good_out = StorePath::make_fixed_output("src.tar.gz", &nh, false, &[]).unwrap();
    let good_aterm = format!(
        r#"Derive([("out","{p}","sha256","{zero_hash}")],[],[],"builtin","builtin:fetchurl",[],[("out","{p}")])"#,
        p = good_out.as_str(),
    );
    let drv = NixDerivation::parse(&good_aterm).unwrap();
    let p = to_proto(&drv);
    let enc = canonical_encode(&p);
    let digest = derivation_digest(&p);
    let path = mint_drv_path(&drv, &good_aterm, "src.tar.gz.drv");
    let v = verify_drv_blob(&enc, &digest, &path).expect("correct FOD verifies");
    assert_eq!(v.aterm, good_aterm);
}

/// Seed regenerator for the `drv_proto_decode` fuzz target: canonical
/// encodings of the two golden fixtures plus every ATerm seed of the
/// `derivation_parsing` target. `#[ignore]`d like the `.fields`
/// regenerators — run explicitly when the schema or the fixtures
/// change:
///
/// ```sh
/// cargo test -p rio-proto --test derivation_codec -- --ignored regenerate_drv_proto_fuzz_seeds
/// ```
#[test]
#[ignore = "seed regenerator, not a test — writes fuzz/rio-nix/corpus/drv_proto_decode/"]
fn regenerate_drv_proto_fuzz_seeds() {
    let manifest = std::env::var("CARGO_MANIFEST_DIR").expect("set by cargo/nextest");
    let fuzz_corpus = std::path::Path::new(&manifest).join("../fuzz/rio-nix/corpus");
    let out_dir = fuzz_corpus.join("drv_proto_decode");
    std::fs::create_dir_all(&out_dir).unwrap();

    let write_seed = |name: &str, aterm: &str| {
        let drv = NixDerivation::parse(aterm).expect("seed source parses");
        let p = to_proto(&drv);
        validate_derivation(&p).unwrap();
        std::fs::write(out_dir.join(name), canonical_encode(&p)).unwrap();
    };

    write_seed("seed-structured-attrs.bin", STRUCTURED_ATERM);
    write_seed("seed-non-ascii.bin", NON_ASCII_ATERM);

    let aterm_seeds = fuzz_corpus.join("derivation_parsing");
    for e in std::fs::read_dir(&aterm_seeds).unwrap() {
        let path = e.unwrap().path();
        let Some(stem) = path
            .file_name()
            .and_then(|n| n.to_str())
            .and_then(|n| n.strip_suffix(".drv"))
        else {
            continue;
        };
        let aterm = std::fs::read_to_string(&path).unwrap();
        write_seed(&format!("{stem}.bin"), &aterm);
    }
}
