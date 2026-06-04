//! Store-side derivation modulo cache (`store.ingest.drv-modulo-cache+2`).
//!
//! The cache is rio's persistent `drvHashes`/`pathDerivationModulo`
//! (CppNix `derivations.cc:856-874`): populated best-effort at `.drv`
//! ingestion from the store's OWN bytes, keyed by the text-CA-bound
//! path, never from a client claim. The golden corpus
//! (`rio-nix/tests/fixtures/drv/`, CppNix-minted) is the parity ground
//! truth: real `nix-instantiate` paths, real chain hashes.

use super::*;
use rio_auth::hmac::{HmacSigner, ServiceClaims};

const TEST_KEY: &[u8] = b"test-key-at-least-32-bytes-long!";
const SERVICE_KEY: &[u8] = b"test-service-hmac-key-32-bytes!!!!";

// The files under `fixtures/` are byte-identical vendored copies of
// rio-nix/tests/fixtures/drv/ (source of truth, where nix minted them).
// They are immutable by construction — the store-path file names embed
// the content hash and these tests depend on path↔content text-CA
// agreement, so any edit breaks both crates' suites loudly. Vendoring
// (rather than a `../../../rio-nix/...` include) keeps the include
// inside this crate's source tree: crate2nix builds each crate from a
// filtered per-crate source where sibling crates' files do not exist.
const GOLDEN_LEAF_PATH: &str = "/nix/store/ragyx33c7zn1kxaag6nc57aiw71699ln-rio-golden-leaf.drv";
const GOLDEN_LEAF: &str =
    include_str!("fixtures/ragyx33c7zn1kxaag6nc57aiw71699ln-rio-golden-leaf.drv");
const GOLDEN_MULTI_PATH: &str = "/nix/store/jkafcgv3rmnnyrhbr0zmfmh58fnw8wgw-rio-golden-multi.drv";
const GOLDEN_MULTI: &str =
    include_str!("fixtures/jkafcgv3rmnnyrhbr0zmfmh58fnw8wgw-rio-golden-multi.drv");
const GOLDEN_FOD_PATH: &str = "/nix/store/5d0dlxwjfzi5pbqb526pd35ny1rcmm7x-rio-golden-fod.drv";
const GOLDEN_FOD: &str =
    include_str!("fixtures/5d0dlxwjfzi5pbqb526pd35ny1rcmm7x-rio-golden-fod.drv");
const GOLDEN_CONSUMER_PATH: &str =
    "/nix/store/92fkmfw4x3ks4dl3pvhk9s0hm3z30cc2-rio-golden-consumer.drv";
const GOLDEN_CONSUMER: &str =
    include_str!("fixtures/92fkmfw4x3ks4dl3pvhk9s0hm3z30cc2-rio-golden-consumer.drv");

fn now_unix() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs()
}

fn service_token() -> String {
    let claims = ServiceClaims {
        caller: "rio-gateway".into(),
        expiry_unix: now_unix() + 60,
    };
    HmacSigner::from_key(SERVICE_KEY.to_vec()).sign(&claims)
}

/// Upload a `.drv` at its ORIGINAL (CppNix-minted) path with the given
/// references — the text-CA gate re-derives and must agree.
async fn upload_drv_at(
    s: &mut StoreSession,
    path: &str,
    text: &str,
    refs: &[&str],
) -> anyhow::Result<bool> {
    let text = text.trim_end();
    let node = rio_nix::nar::NarNode::Regular {
        executable: false,
        contents: text.as_bytes().to_vec(),
    };
    let mut nar = Vec::new();
    rio_nix::nar::serialize(&mut nar, &node)?;
    let mut info = make_path_info_for_nar(path, &nar);
    info.references = refs
        .iter()
        .map(|r| rio_nix::store_path::StorePath::parse(r))
        .collect::<Result<_, _>>()?;
    Ok(put_path_with_header(
        &mut s.client,
        info,
        nar,
        rio_proto::SERVICE_TOKEN_HEADER,
        &service_token(),
    )
    .await?)
}

async fn cache_row(
    pool: &sqlx::PgPool,
    drv_path: &str,
) -> anyhow::Result<Option<(Vec<u8>, serde_json::Value, bool)>> {
    let key: Vec<u8> = {
        use sha2::Digest as _;
        sha2::Sha256::digest(drv_path.as_bytes()).to_vec()
    };
    Ok(sqlx::query_as(
        "SELECT modulo_hash, ia_output_paths, deferred FROM drv_modulo_cache \
         WHERE drv_path_hash = $1",
    )
    .bind(key)
    .fetch_optional(pool)
    .await?)
}

// r[verify store.ingest.drv-modulo-cache+2]
/// Golden parity: ingesting a leaf `.drv` populates a row whose modulo
/// hash equals `rio_nix::hash_derivation_modulo` over the same bytes,
/// and whose IA output map equals the CppNix-minted declared paths
/// (declared == derived is the corpus's own ground truth).
#[tokio::test]
async fn ingest_populates_modulo_row_with_golden_parity() -> TestResult {
    let mut s =
        StoreSession::new_with_service_hmac(TEST_KEY.to_vec(), SERVICE_KEY.to_vec()).await?;
    assert!(upload_drv_at(&mut s, GOLDEN_LEAF_PATH, GOLDEN_LEAF, &[]).await?);

    let (modulo, paths, deferred) = cache_row(&s.db.pool, GOLDEN_LEAF_PATH)
        .await?
        .expect("leaf row populated at ingestion");

    let drv = rio_nix::derivation::Derivation::parse(GOLDEN_LEAF.trim_end()).unwrap();
    let resolve = |_: &str| -> Option<&rio_nix::derivation::Derivation> { None };
    let mut cache = std::collections::HashMap::new();
    let expected =
        rio_nix::derivation::hash_derivation_modulo(&drv, GOLDEN_LEAF_PATH, &resolve, &mut cache)
            .unwrap();
    assert_eq!(modulo.as_slice(), expected.as_slice(), "modulo parity");
    assert!(!deferred);
    let declared: std::collections::HashMap<String, String> = drv
        .outputs()
        .iter()
        .map(|o| (o.name().to_string(), o.path().to_string()))
        .collect();
    let cached: std::collections::HashMap<String, String> = paths
        .as_object()
        .unwrap()
        .iter()
        .map(|(k, v)| (k.clone(), v.as_str().unwrap().to_string()))
        .collect();
    assert_eq!(cached, declared, "static-IA paths derived == declared");
    Ok(())
}

// r[verify store.ingest.drv-modulo-cache+2]
/// Chain ingestion: with leaf + multi + fod resident, the consumer's
/// row derives from CACHE-SEEDED input hashes and must equal the
/// full-resolution walk over the in-memory corpus — the cache-seeding
/// shortcut is hash-equivalent to resolving every input.
#[tokio::test]
async fn chain_ingestion_seeds_inputs_from_cache_rows() -> TestResult {
    let mut s =
        StoreSession::new_with_service_hmac(TEST_KEY.to_vec(), SERVICE_KEY.to_vec()).await?;
    assert!(upload_drv_at(&mut s, GOLDEN_LEAF_PATH, GOLDEN_LEAF, &[]).await?);
    assert!(upload_drv_at(&mut s, GOLDEN_MULTI_PATH, GOLDEN_MULTI, &[]).await?);
    assert!(upload_drv_at(&mut s, GOLDEN_FOD_PATH, GOLDEN_FOD, &[]).await?);
    assert!(
        upload_drv_at(
            &mut s,
            GOLDEN_CONSUMER_PATH,
            GOLDEN_CONSUMER,
            &[GOLDEN_FOD_PATH, GOLDEN_LEAF_PATH, GOLDEN_MULTI_PATH],
        )
        .await?
    );

    let (modulo, paths, _) = cache_row(&s.db.pool, GOLDEN_CONSUMER_PATH)
        .await?
        .expect("consumer row populated (inputs were cache-resident)");

    // Full-resolution reference walk over the in-memory corpus.
    let leaf = rio_nix::derivation::Derivation::parse(GOLDEN_LEAF.trim_end()).unwrap();
    let multi = rio_nix::derivation::Derivation::parse(GOLDEN_MULTI.trim_end()).unwrap();
    let fod = rio_nix::derivation::Derivation::parse(GOLDEN_FOD.trim_end()).unwrap();
    let consumer = rio_nix::derivation::Derivation::parse(GOLDEN_CONSUMER.trim_end()).unwrap();
    let resolve = |p: &str| -> Option<&rio_nix::derivation::Derivation> {
        match p {
            GOLDEN_LEAF_PATH => Some(&leaf),
            GOLDEN_MULTI_PATH => Some(&multi),
            GOLDEN_FOD_PATH => Some(&fod),
            _ => None,
        }
    };
    let mut cache = std::collections::HashMap::new();
    let expected = rio_nix::derivation::hash_derivation_modulo(
        &consumer,
        GOLDEN_CONSUMER_PATH,
        &resolve,
        &mut cache,
    )
    .unwrap();
    assert_eq!(
        modulo.as_slice(),
        expected.as_slice(),
        "cache-seeded modulo == full-resolution modulo"
    );
    // Consumer is static IA: derived paths equal its declared ones.
    let declared: std::collections::HashMap<String, String> = consumer
        .outputs()
        .iter()
        .map(|o| (o.name().to_string(), o.path().to_string()))
        .collect();
    let cached: std::collections::HashMap<String, String> = paths
        .as_object()
        .unwrap()
        .iter()
        .map(|(k, v)| (k.clone(), v.as_str().unwrap().to_string()))
        .collect();
    assert_eq!(cached, declared);
    Ok(())
}

// r[verify store.ingest.drv-modulo-cache+2]
/// Out-of-order upload: a consumer arriving before its inputs SKIPS
/// population (no row) but the upload itself succeeds — read-through at
/// proof time completes the chain later.
#[tokio::test]
async fn out_of_order_upload_skips_population_without_failing() -> TestResult {
    let mut s =
        StoreSession::new_with_service_hmac(TEST_KEY.to_vec(), SERVICE_KEY.to_vec()).await?;
    assert!(
        upload_drv_at(
            &mut s,
            GOLDEN_CONSUMER_PATH,
            GOLDEN_CONSUMER,
            &[GOLDEN_FOD_PATH, GOLDEN_LEAF_PATH, GOLDEN_MULTI_PATH],
        )
        .await?,
        "upload succeeds regardless of population"
    );
    assert!(
        cache_row(&s.db.pool, GOLDEN_CONSUMER_PATH).await?.is_none(),
        "no row: inputs absent, population skipped"
    );
    Ok(())
}

// r[verify store.ingest.drv-modulo-cache+2]
/// Fixed-output derivers cache their modulo hash with an EMPTY IA map
/// (their paths derive from the declared content hash, not the input
/// walk); floating-CA derivers are marked deferred.
#[tokio::test]
async fn fod_and_floating_rows_have_correct_shapes() -> TestResult {
    let mut s =
        StoreSession::new_with_service_hmac(TEST_KEY.to_vec(), SERVICE_KEY.to_vec()).await?;
    assert!(upload_drv_at(&mut s, GOLDEN_FOD_PATH, GOLDEN_FOD, &[]).await?);
    let (_, paths, deferred) = cache_row(&s.db.pool, GOLDEN_FOD_PATH)
        .await?
        .expect("FOD row populated");
    assert_eq!(paths, serde_json::json!({}), "FOD: empty IA map");
    assert!(!deferred, "FOD paths are statically known");

    // Floating-CA leaf (synthetic; uploaded at its own text-CA path).
    let floating = r#"Derive([("out","","r:sha256","")],[],[],"x86_64-linux","/bin/sh",["-c","echo f > $out"],[("name","floaty")])"#;
    let (path, nar) =
        rio_test_support::fixtures::make_drv_nar("floaty.drv", floating.as_bytes(), &[]);
    let info = make_path_info_for_nar(&path, &nar);
    assert!(
        put_path_with_header(
            &mut s.client,
            info,
            nar,
            rio_proto::SERVICE_TOKEN_HEADER,
            &service_token(),
        )
        .await?
    );
    let (_, paths, deferred) = cache_row(&s.db.pool, &path)
        .await?
        .expect("floating row populated");
    assert_eq!(paths, serde_json::json!({}));
    assert!(deferred, "floating-CA derivers are deferred");
    Ok(())
}

// r[verify store.ingest.drv-modulo-cache+2]
/// Ordering pin: the text-CA gate runs FIRST — bytes claimed at a
/// non-matching `.drv` path are rejected before the population hook can
/// see them; and text-CA-valid garbage uploads fine but populates
/// nothing (parse_failed skip).
#[tokio::test]
async fn text_ca_gate_runs_before_population_and_garbage_skips() -> TestResult {
    let mut s =
        StoreSession::new_with_service_hmac(TEST_KEY.to_vec(), SERVICE_KEY.to_vec()).await?;

    // (a) Real bytes at a WRONG path: rejected, no row anywhere.
    let wrong_path = format!("/nix/store/{}-wrong.drv", "a".repeat(32));
    assert!(
        upload_drv_at(&mut s, &wrong_path, GOLDEN_LEAF, &[])
            .await
            .is_err(),
        "text-CA mismatch rejects the upload"
    );
    assert!(cache_row(&s.db.pool, &wrong_path).await?.is_none());

    // (b) Garbage at its own text-CA path: upload OK, population skips.
    let garbage = b"this is not a derivation";
    let (path, nar) = rio_test_support::fixtures::make_drv_nar("garbage.drv", garbage, &[]);
    let info = make_path_info_for_nar(&path, &nar);
    assert!(
        put_path_with_header(
            &mut s.client,
            info,
            nar,
            rio_proto::SERVICE_TOKEN_HEADER,
            &service_token(),
        )
        .await?
    );
    assert!(
        cache_row(&s.db.pool, &path).await?.is_none(),
        "unparseable bytes populate nothing"
    );
    Ok(())
}

// ---------------------------------------------------------------------------
// C1c1 (merged_bug_015): error classes never fold into the absent verdict
// ---------------------------------------------------------------------------

/// IA assignment claims naming `deriver` (not fixed-output, not CA) —
/// the shape that triggers the deriver-proof gate.
fn ia_claims_token(deriver: &str, outputs: Vec<String>) -> String {
    let claims = rio_auth::hmac::AssignmentClaims {
        executor_id: "w-test".into(),
        drv_hash: deriver.into(),
        expected_outputs: outputs,
        expiry_unix: now_unix() + 60,
        is_ca: false,
        is_fixed_output: false,
        tenant: None,
    };
    HmacSigner::from_key(TEST_KEY.to_vec()).sign(&claims)
}

// r[verify store.put.ia-deriver-proof+4]
/// Row corruption is an INTERNAL error, never an authorization verdict:
/// a `status='complete'` deriver `.drv` whose inline NAR does not parse
/// (text-CA-gated at ingestion, so this is row corruption) must surface
/// as `INTERNAL` — pre-fix, the `.ok()??` fold reported it as
/// `PERMISSION_DENIED` "deriver closure unverifiable", telling an
/// honest worker its deriver doesn't derive its path because a database
/// row rotted.
#[tokio::test]
async fn corrupt_inline_drv_nar_yields_internal_not_permission_denied() -> TestResult {
    let mut s =
        StoreSession::new_with_service_hmac(TEST_KEY.to_vec(), SERVICE_KEY.to_vec()).await?;
    // Resident deriver, then corrupt its inline NAR and drop its cache
    // row so the proof must read the (now corrupt) own bytes.
    assert!(upload_drv_at(&mut s, GOLDEN_LEAF_PATH, GOLDEN_LEAF, &[]).await?);
    let key: Vec<u8> = {
        use sha2::Digest as _;
        sha2::Sha256::digest(GOLDEN_LEAF_PATH.as_bytes()).to_vec()
    };
    sqlx::query("DELETE FROM drv_modulo_cache WHERE drv_path_hash = $1")
        .bind(&key)
        .execute(&s.db.pool)
        .await?;
    let n = sqlx::query(
        "UPDATE manifests SET inline_blob = $2 WHERE store_path_hash = \
         (SELECT store_path_hash FROM narinfo WHERE store_path = $1)",
    )
    .bind(GOLDEN_LEAF_PATH)
    .bind(b"garbage, not a NAR".as_slice())
    .execute(&s.db.pool)
    .await?
    .rows_affected();
    assert_eq!(
        n, 1,
        "fixture: corrupted exactly the deriver's manifest row"
    );

    // IA upload claiming an output of the (corrupt) deriver.
    let (nar, _) = make_nar(b"ia output bytes");
    let out_path = test_store_path("c1c1-out");
    let info = make_path_info_for_nar(&out_path, &nar);
    let token = ia_claims_token(GOLDEN_LEAF_PATH, vec![out_path.clone()]);
    let err = put_path_with_token(&mut s.client, info, nar, &token)
        .await
        .expect_err("corrupt deriver row must fail the upload");
    assert_eq!(
        err.code(),
        tonic::Code::Internal,
        "row corruption is INTERNAL, not an authorization verdict: {err:?}"
    );
    Ok(())
}

// r[verify store.put.ia-deriver-proof+4]
/// Duplicate-output derivation bytes fail the deriver proof CLOSED:
/// the parse boundary (nix.drv.type-classify+1, fail-closed divergence
/// #4) rejects duplicates, so the walk reports the deriver as
/// `Absent(Unparseable)` -> `PERMISSION_DENIED` naming the path. The
/// duplicate deriver is never deferred-exempt (its outputs are
/// concrete) and never contributes a partially-collapsed output
/// allowlist -- the collapse semantics that made name-keyed views
/// silently partial are unreachable past the parser (merged_bug_072,
/// store layer; test-only, falls out of C5c1 + C1c3).
#[tokio::test]
async fn duplicate_output_deriver_fails_proof_closed() -> TestResult {
    let mut s =
        StoreSession::new_with_service_hmac(TEST_KEY.to_vec(), SERVICE_KEY.to_vec()).await?;

    // An otherwise-plausible Derive ATerm whose two outputs share the
    // name "out". The text-CA gate accepts the UPLOAD (any bytes at
    // their own text-CA path), but parse-time classification rejects
    // the shape, so cache population skips and the proof fails closed.
    let dup = format!(
        "Derive([(\"out\",\"/nix/store/{p1}-dup-out\",\"\",\"\"),\
         (\"out\",\"/nix/store/{p2}-dup-out\",\"\",\"\")],[],[],\
         \"x86_64-linux\",\"/bin/sh\",[],[(\"k\",\"v\")])",
        p1 = "b".repeat(32),
        p2 = "c".repeat(32),
    );
    let (drv_path, drv_nar) =
        rio_test_support::fixtures::make_drv_nar("dup-out.drv", dup.as_bytes(), &[]);
    let info = make_path_info_for_nar(&drv_path, &drv_nar);
    assert!(
        put_path_with_header(
            &mut s.client,
            info,
            drv_nar,
            rio_proto::SERVICE_TOKEN_HEADER,
            &service_token(),
        )
        .await?,
        "upload itself is text-CA-clean"
    );
    assert!(
        cache_row(&s.db.pool, &drv_path).await?.is_none(),
        "duplicate-output bytes must not populate the modulo cache"
    );

    // IA upload claiming an output of the duplicate-output deriver: the
    // read-through parses the store's own bytes and fails CLOSED.
    let (nar, _) = make_nar(b"claimed output bytes");
    let out_path = test_store_path("dup-claimed-out");
    let info = make_path_info_for_nar(&out_path, &nar);
    let token = ia_claims_token(&drv_path, vec![out_path.clone()]);
    let err = put_path_with_token(&mut s.client, info, nar, &token)
        .await
        .expect_err("duplicate-output deriver must fail the proof");
    assert_eq!(
        err.code(),
        tonic::Code::PermissionDenied,
        "closure verdict, not infra: {err:?}"
    );
    assert!(
        err.message().contains("cannot be used"),
        "names the unusable deriver: {}",
        err.message()
    );
    Ok(())
}

// r[verify store.put.ia-deriver-proof+4]
/// The residency clause end-to-end: after the deriver `.drv` is GC'd
/// (narinfo gone) its proof row SURVIVES, and an IA upload claiming its
/// output is still ACCEPTED — "previously verified against resident
/// bytes" is durable, exactly as the gate doc and rejection text state.
/// Only deleting the proof row itself (operator invalidation) makes the
/// upload fail, and then with the NotResident wording that tells the
/// worker what to upload.
#[tokio::test]
async fn ia_proof_survives_deriver_gc() -> TestResult {
    let mut s =
        StoreSession::new_with_service_hmac(TEST_KEY.to_vec(), SERVICE_KEY.to_vec()).await?;
    assert!(upload_drv_at(&mut s, GOLDEN_LEAF_PATH, GOLDEN_LEAF, &[]).await?);
    assert!(
        cache_row(&s.db.pool, GOLDEN_LEAF_PATH).await?.is_some(),
        "ingestion populated the proof row"
    );

    // Simulate deriver GC: narinfo (and CASCADE) gone, proof row kept —
    // the exact state sweep_preserves_drv_modulo_rows pins at the unit
    // level.
    let key: Vec<u8> = {
        use sha2::Digest as _;
        sha2::Sha256::digest(GOLDEN_LEAF_PATH.as_bytes()).to_vec()
    };
    sqlx::query("DELETE FROM narinfo WHERE store_path_hash = $1")
        .bind(&key)
        .execute(&s.db.pool)
        .await?;

    // The leaf's sole IA output, straight from the golden fixture.
    let out_path = {
        let drv = rio_nix::derivation::Derivation::parse(GOLDEN_LEAF.trim_end())
            .expect("golden leaf parses");
        drv.outputs()[0].path().to_string()
    };
    let (nar, _) = make_nar(b"post-gc output bytes");
    let info = make_path_info_for_nar(&out_path, &nar);
    let token = ia_claims_token(GOLDEN_LEAF_PATH, vec![out_path.clone()]);
    assert!(
        put_path_with_token(&mut s.client, info, nar.clone(), &token).await?,
        "proof from the surviving row accepts the upload after deriver GC"
    );

    // Remove the upload AND the proof row (operator invalidation
    // territory): now the proof has nothing to work from.
    sqlx::query("DELETE FROM narinfo")
        .execute(&s.db.pool)
        .await?;
    sqlx::query("DELETE FROM drv_modulo_cache WHERE drv_path_hash = $1")
        .bind(&key)
        .execute(&s.db.pool)
        .await?;
    let info = make_path_info_for_nar(&out_path, &nar);
    let err = put_path_with_token(&mut s.client, info, nar, &token)
        .await
        .expect_err("no resident deriver and no proof row: fail closed");
    assert_eq!(err.code(), tonic::Code::PermissionDenied, "got: {err:?}");
    assert!(
        err.message().contains("not resident"),
        "NotResident wording names the remediation: {}",
        err.message()
    );
    Ok(())
}

// r[verify store.put.ia-deriver-proof+4]
/// Database failure during the proof lookup is an INTERNAL error, never
/// `PERMISSION_DENIED`: with the cache table dropped, the proof's own
/// SELECT fails — pre-fix the `.ok()??` fold reported "deriver closure
/// unverifiable" for what was an infrastructure failure.
#[tokio::test]
async fn proof_db_error_yields_internal_not_permission_denied() -> TestResult {
    let mut s = StoreSession::new_with_hmac(TEST_KEY.to_vec()).await?;
    sqlx::query("DROP TABLE drv_modulo_cache CASCADE")
        .execute(&s.db.pool)
        .await?;

    let (nar, _) = make_nar(b"ia output bytes 2");
    let out_path = test_store_path("c1c1-dberr-out");
    let deriver = format!("/nix/store/{}-absent.drv", "c".repeat(32));
    let info = make_path_info_for_nar(&out_path, &nar);
    let token = ia_claims_token(&deriver, vec![out_path.clone()]);
    let err = put_path_with_token(&mut s.client, info, nar, &token)
        .await
        .expect_err("DB failure must fail the upload");
    assert_eq!(
        err.code(),
        tonic::Code::Internal,
        "infrastructure failure is INTERNAL, not an authorization verdict: {err:?}"
    );
    Ok(())
}

// ---------------------------------------------------------------------------
// C1c3 (merged_bug_002 DEPLOY-BLOCKER + bug_007): budgeted monotone walk
// ---------------------------------------------------------------------------

mod proof_walk {
    use super::*;
    use rio_nix::derivation::{Derivation, input_addressed_output_paths};
    use rio_nix::hash::{HashAlgo, NixHash};
    use rio_nix::store_path::StorePath;
    use rio_store::test_helpers::{PROOF_WALK_WORK_MAX_FOR_TESTS, proof_walk_for_tests};
    use sha2::{Digest as _, Sha256};
    use std::collections::HashMap;

    /// Mint a VALID static-IA `.drv` (declared == derived out path)
    /// whose inputs are previously-minted nodes. Returns the text-CA
    /// drv path; the text + parsed drv land in `minted`.
    fn mint_node(
        tag: &str,
        inputs: &[String],
        minted: &mut HashMap<String, (String, Derivation)>,
        derive_cache: &mut HashMap<String, [u8; 32]>,
    ) -> String {
        mint_node_padded(tag, inputs, 0, minted, derive_cache)
    }

    /// [`mint_node`] with `pad` bytes of env padding — the byte-flood
    /// shape (bug_079): a VALID derivation whose `.drv` text is
    /// arbitrarily large while costing the same WORK units to walk.
    fn mint_node_padded(
        tag: &str,
        inputs: &[String],
        pad: usize,
        minted: &mut HashMap<String, (String, Derivation)>,
        derive_cache: &mut HashMap<String, [u8; 32]>,
    ) -> String {
        let inputs_aterm: String = inputs
            .iter()
            .map(|p| format!(r#"("{p}",["out"])"#))
            .collect::<Vec<_>>()
            .join(",");
        let padding = "p".repeat(pad);
        let build = |out: &str| {
            format!(
                r#"Derive([("out","{out}","","")],[{inputs_aterm}],[],"x86_64-linux","/bin/sh",["-c","x"],[("name","{tag}"),("out","{out}"),("pad","{padding}")])"#
            )
        };
        let masked = Derivation::parse(&build("")).expect("masked parses");
        let name_only = format!("/nix/store/{}-{tag}.drv", "a".repeat(32));
        let resolve = |p: &str| -> Option<&Derivation> { minted.get(p).map(|(_, d)| d) };
        let paths = input_addressed_output_paths(&masked, &name_only, &resolve, derive_cache)
            .expect("derives");
        let out = paths["out"].as_str().to_owned();
        let text = build(&out);
        let h = NixHash::new(HashAlgo::SHA256, Sha256::digest(text.as_bytes()).to_vec()).unwrap();
        let refs: Vec<&str> = inputs.iter().map(String::as_str).collect();
        let parsed_refs: Vec<StorePath> =
            refs.iter().map(|r| StorePath::parse(r).unwrap()).collect();
        let drv_path = StorePath::make_text(&format!("{tag}.drv"), &h, &parsed_refs)
            .unwrap()
            .as_str()
            .to_owned();
        let parsed = Derivation::parse(&text).expect("final parses");
        minted.insert(drv_path.clone(), (text, parsed));
        drv_path
    }

    /// Stage a minted `.drv` directly as a complete inline manifest
    /// (the walk's input contract is DB rows; gRPC ingestion is covered
    /// by the ingest tests). Does NOT populate the modulo cache.
    async fn stage_drv_sql(pool: &sqlx::PgPool, path: &str, text: &str) -> anyhow::Result<()> {
        let key = StorePath::parse(path)?.sha256_digest();
        let node = rio_nix::nar::NarNode::Regular {
            executable: false,
            contents: text.as_bytes().to_vec(),
        };
        let mut nar = Vec::new();
        rio_nix::nar::serialize(&mut nar, &node)?;
        sqlx::query(
            "INSERT INTO narinfo (store_path_hash, store_path, nar_hash, nar_size) \
             VALUES ($1, $2, $3, $4) ON CONFLICT (store_path_hash) DO NOTHING",
        )
        .bind(key.as_slice())
        .bind(path)
        .bind([0u8; 32].as_slice())
        .bind(nar.len() as i64)
        .execute(pool)
        .await?;
        sqlx::query(
            "INSERT INTO manifests (store_path_hash, status, inline_blob) \
             VALUES ($1, 'complete', $2) ON CONFLICT (store_path_hash) \
             DO UPDATE SET status = 'complete', inline_blob = $2",
        )
        .bind(key.as_slice())
        .bind(&nar)
        .execute(pool)
        .await?;
        Ok(())
    }

    async fn cache_rows(pool: &sqlx::PgPool) -> i64 {
        sqlx::query_scalar("SELECT count(*)::bigint FROM drv_modulo_cache")
            .fetch_one(pool)
            .await
            .unwrap()
    }

    // r[verify store.put.ia-deriver-proof+4]
    /// A 300-deep pure chain — 9.4× past the OLD depth bound of 32 that
    /// rejected the measured real-world class — converges in one cold
    /// walk with every row persisted.
    #[tokio::test]
    async fn deep_chain_300_converges() -> TestResult {
        let s = StoreSession::new().await?;
        let mut minted = HashMap::new();
        let mut dc = HashMap::new();
        let mut prev: Vec<String> = vec![];
        let mut last = String::new();
        for i in 0..300 {
            last = mint_node(&format!("chain{i}"), &prev, &mut minted, &mut dc);
            prev = vec![last.clone()];
        }
        for (path, (text, _)) in &minted {
            stage_drv_sql(&s.db.pool, path, text).await?;
        }
        let report =
            proof_walk_for_tests(&s.db.pool, None, &last, PROOF_WALK_WORK_MAX_FOR_TESTS).await?;
        assert!(report.proven, "chain proves: {report:?}");
        assert_eq!(cache_rows(&s.db.pool).await, 300, "every row persisted");
        assert!(
            report.work_used < 1_000,
            "ops counted, not free: {}",
            report.work_used
        );
        Ok(())
    }

    // r[verify store.put.ia-deriver-proof+4]
    /// THE deploy-blocker merge gate (merged_bug_002): a 2,048-node
    /// closure at the measured real-world shape class (spine depth
    /// 2,048 ≥ the measured 236; cross edges for fan) proves in ONE
    /// cold walk with work_used ≤ cap/2 — structural op counting, no
    /// wall-clock. Pre-fix, READ_THROUGH_MAX_DEPTH=32 rejected every
    /// closure of this class outright.
    #[tokio::test]
    async fn real_scale_closure_2048_converges_with_headroom() -> TestResult {
        let s = StoreSession::new().await?;
        let mut minted = HashMap::new();
        let mut dc = HashMap::new();
        let mut paths: Vec<String> = Vec::with_capacity(2048);
        for i in 0..2048usize {
            let mut inputs = vec![];
            if i >= 1 {
                inputs.push(paths[i - 1].clone());
            }
            if i >= 17 {
                inputs.push(paths[i - 17].clone());
            }
            paths.push(mint_node(&format!("rs{i}"), &inputs, &mut minted, &mut dc));
        }
        for (path, (text, _)) in &minted {
            stage_drv_sql(&s.db.pool, path, text).await?;
        }
        let target = paths.last().unwrap();
        let report =
            proof_walk_for_tests(&s.db.pool, None, target, PROOF_WALK_WORK_MAX_FOR_TESTS).await?;
        assert!(report.proven, "real-scale closure proves: {report:?}");
        assert_eq!(cache_rows(&s.db.pool).await, 2048);
        eprintln!(
            "real-scale 2048-node walk: work_used = {}",
            report.work_used
        );
        assert!(
            report.work_used <= PROOF_WALK_WORK_MAX_FOR_TESTS / 2,
            "headroom gate: work_used {} > cap/2 {}",
            report.work_used,
            PROOF_WALK_WORK_MAX_FOR_TESTS / 2
        );
        Ok(())
    }

    // r[verify store.put.ia-deriver-proof+4]
    /// Over-budget exits persist what they proved, and identical
    /// retries CONVERGE instead of re-failing forever (the
    /// merged_bug_002 zero-progress pathology): a 60-leaf star under a
    /// cap of 30 needs several attempts, each persisting more leaves,
    /// until the root proves.
    #[tokio::test]
    async fn over_budget_persists_monotone_progress() -> TestResult {
        let s = StoreSession::new().await?;
        let mut minted = HashMap::new();
        let mut dc = HashMap::new();
        let leaves: Vec<String> = (0..60)
            .map(|i| mint_node(&format!("leaf{i}"), &[], &mut minted, &mut dc))
            .collect();
        let root = mint_node("star-root", &leaves, &mut minted, &mut dc);
        for (path, (text, _)) in &minted {
            stage_drv_sql(&s.db.pool, path, text).await?;
        }

        let mut attempts = 0;
        let mut last_rows = 0i64;
        loop {
            attempts += 1;
            let report = proof_walk_for_tests(&s.db.pool, None, &root, 30).await?;
            let rows = cache_rows(&s.db.pool).await;
            if report.proven {
                break;
            }
            assert_eq!(report.reason, Some("over_budget"), "{report:?}");
            assert!(
                rows > last_rows,
                "every over-budget attempt must persist NEW progress \
                 (attempt {attempts}: {last_rows} -> {rows})"
            );
            // merged_bug_086 pin: the reported count IS the SQL row
            // delta — eager computes and the exit drain both route
            // through the owner's sole persist chokepoint.
            assert_eq!(
                report.persisted as i64,
                rows - last_rows,
                "persisted must equal the SQL row delta \
                 (attempt {attempts}: reported {}, delta {})",
                report.persisted,
                rows - last_rows
            );
            last_rows = rows;
            assert!(attempts < 10, "must converge, not livelock");
        }
        assert_eq!(cache_rows(&s.db.pool).await, 61);
        assert!(
            attempts >= 2,
            "fixture must actually exercise resumption (got {attempts} attempts)"
        );
        Ok(())
    }

    // r[verify store.put.ia-deriver-proof+4]
    /// THE bug_083 oracle-parity cut: the walk must NOT descend below a
    /// fixed-output node (`hashDerivationModulo`'s FOD base case,
    /// derivations.cc:864-874 — the `fixed:out:…` fingerprint is
    /// derived from the FOD's own declaration; inputs are never
    /// visited). Fixture: an IA parent whose input is a FOD whose own
    /// input (`ghost`) is NOT resident anywhere. Pre-fix the walk
    /// demanded ghost and the whole proof failed
    /// `NotResident{ghost}` — a verdict the oracle cannot produce.
    #[tokio::test]
    async fn fod_inputs_are_never_required_resident() -> TestResult {
        let s = StoreSession::new().await?;
        let mut minted = HashMap::new();
        let mut dc = HashMap::new();

        let ghost = format!("/nix/store/{}-ghost.drv", "a".repeat(32));
        let fod_text = format!(
            r#"Derive([("out","/nix/store/hf9x46xx06qmkj0ivfqdswgi2qzd2cwz-fixed","sha256","e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855")],[("{ghost}",["out"])],[],"x86_64-linux","/bin/sh",["-c","echo"],[("name","fixed"),("out","/nix/store/hf9x46xx06qmkj0ivfqdswgi2qzd2cwz-fixed"),("outputHash","e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"),("outputHashAlgo","sha256"),("system","x86_64-linux")])"#
        );
        let h = NixHash::new(
            HashAlgo::SHA256,
            Sha256::digest(fod_text.as_bytes()).to_vec(),
        )
        .unwrap();
        let fod_path = StorePath::make_text("fod-cut.drv", &h, &[])
            .unwrap()
            .as_str()
            .to_owned();
        minted.insert(
            fod_path.clone(),
            (fod_text.clone(), Derivation::parse(&fod_text).unwrap()),
        );
        let parent = mint_node(
            "fodcut-parent",
            std::slice::from_ref(&fod_path),
            &mut minted,
            &mut dc,
        );

        // Stage parent + FOD only. ghost is resident NOWHERE.
        for (path, (text, _)) in &minted {
            stage_drv_sql(&s.db.pool, path, text).await?;
        }

        let report =
            proof_walk_for_tests(&s.db.pool, None, &parent, PROOF_WALK_WORK_MAX_FOR_TESTS).await?;
        assert!(
            report.proven,
            "IA parent of a FOD proves without the FOD's inputs: {report:?}"
        );
        assert_eq!(
            cache_rows(&s.db.pool).await,
            2,
            "exactly parent + FOD rows; nothing below the FOD is walked"
        );
        let ghost_row: i64 =
            sqlx::query_scalar("SELECT count(*)::bigint FROM drv_modulo_cache WHERE drv_path = $1")
                .bind(&ghost)
                .fetch_one(&s.db.pool)
                .await?;
        assert_eq!(ghost_row, 0, "ghost never derived");

        // The FOD also proves DIRECTLY (membership-only row).
        sqlx::query("DELETE FROM drv_modulo_cache")
            .execute(&s.db.pool)
            .await?;
        let direct =
            proof_walk_for_tests(&s.db.pool, None, &fod_path, PROOF_WALK_WORK_MAX_FOR_TESTS)
                .await?;
        assert!(direct.proven, "FOD proves standalone: {direct:?}");
        Ok(())
    }

    // r[verify store.put.ia-deriver-proof+4]
    /// THE bug_084 exit-class witness: an INFRASTRUCTURE error
    /// mid-discovery must still drain the arena — every subtree whose
    /// inputs completed before the error is persisted on the `Err`
    /// path, exactly as on typed-verdict exits.
    ///
    /// Shape: root{mid_a, mid_b}; each mid has its own leaf. The mid
    /// whose path sorts GREATER pops first (inputs push in BTreeMap
    /// ascending order; the DFS stack pops the last push), so its
    /// branch completes (leaf eager-persisted; mid retained,
    /// drain-eligible) before the OTHER mid is fetched. That other mid
    /// is staged with a CORRUPT inline NAR → own_drv_bytes returns
    /// Err(InvariantViolation) — deterministic, no chunk backend.
    /// Pre-owner-type, the `?` dropped the arena: the completed mid's
    /// row was lost and a retry re-derived it. Post-fix, fail() drains:
    /// the completed mid's row EXISTS after the erroring walk.
    #[tokio::test]
    async fn err_exit_drains_completed_subtrees() -> TestResult {
        let s = StoreSession::new().await?;
        let mut minted = HashMap::new();
        let mut dc = HashMap::new();
        let leaf_a = mint_node("errleaf-a", &[], &mut minted, &mut dc);
        let mid_a = mint_node("errmid-a", &[leaf_a], &mut minted, &mut dc);
        let leaf_b = mint_node("errleaf-b", &[], &mut minted, &mut dc);
        let mid_b = mint_node("errmid-b", &[leaf_b], &mut minted, &mut dc);
        let root = mint_node(
            "err-root",
            &[mid_a.clone(), mid_b.clone()],
            &mut minted,
            &mut dc,
        );

        // Role assignment by SORT ORDER (text-CA path hashes are not
        // name-controllable): the greater path pops FIRST and is the
        // GOOD branch; the lesser pops second and is CORRUPT.
        let (good_mid, corrupt_mid) = if mid_a > mid_b {
            (mid_a.clone(), mid_b.clone())
        } else {
            (mid_b.clone(), mid_a.clone())
        };
        let good_leaf = minted[&good_mid]
            .1
            .input_drvs()
            .keys()
            .next()
            .unwrap()
            .clone();

        for (path, (text, _)) in &minted {
            if *path == corrupt_mid {
                continue;
            }
            stage_drv_sql(&s.db.pool, path, text).await?;
        }
        // Stage the corrupt mid: complete manifest whose inline blob is
        // NOT a NAR → extraction fails → Err (row corruption class).
        let key = StorePath::parse(&corrupt_mid)?.sha256_digest();
        sqlx::query(
            "INSERT INTO narinfo (store_path_hash, store_path, nar_hash, nar_size) \
             VALUES ($1, $2, $3, $4)",
        )
        .bind(key.as_slice())
        .bind(&corrupt_mid)
        .bind([0u8; 32].as_slice())
        .bind(16i64)
        .execute(&s.db.pool)
        .await?;
        sqlx::query(
            "INSERT INTO manifests (store_path_hash, status, inline_blob) \
             VALUES ($1, 'complete', $2)",
        )
        .bind(key.as_slice())
        .bind(b"not a nar at all".as_slice())
        .execute(&s.db.pool)
        .await?;

        let err = proof_walk_for_tests(&s.db.pool, None, &root, PROOF_WALK_WORK_MAX_FOR_TESTS)
            .await
            .expect_err("corrupt resident NAR is an infrastructure Err, not a verdict");
        assert!(
            err.to_string().contains("invariant violation"),
            "row-corruption class carried: {err}"
        );

        // THE PIN: the completed branch survived the Err exit.
        let good_leaf_present: i64 =
            sqlx::query_scalar("SELECT count(*)::bigint FROM drv_modulo_cache WHERE drv_path = $1")
                .bind(&good_leaf)
                .fetch_one(&s.db.pool)
                .await?;
        let good_mid_present: i64 =
            sqlx::query_scalar("SELECT count(*)::bigint FROM drv_modulo_cache WHERE drv_path = $1")
                .bind(&good_mid)
                .fetch_one(&s.db.pool)
                .await?;
        assert_eq!(
            good_leaf_present, 1,
            "eager leaf persisted before the error"
        );
        assert_eq!(
            good_mid_present, 1,
            "DRAIN-ONLY row: the completed mid must be persisted by the \
             Err-path drain (bug_084 — pre-fix this row was dropped \
             with the arena)"
        );
        assert_eq!(
            cache_rows(&s.db.pool).await,
            2,
            "exactly the completed branch persists; the corrupt branch \
             and the blocked root do not"
        );
        Ok(())
    }

    // r[verify store.put.ia-deriver-proof+4]
    /// THE byte-flood scale test (bug_079, R4 SCALE-PER-DIMENSION): a
    /// padded mid-tier (8 mids × 1 MiB env padding, each over its own
    /// leaf) under a 3.5 MiB arena cap. The WORK budget never binds
    /// (each attempt costs ~20 ops against a 16,384 cap) — only the
    /// BYTE co-budget catches the retention. Pre-fix the walk retained
    /// all 8 MiB "within budget"; post-fix each attempt exhausts after
    /// ~3 retained mids, the monotone exit drain persists every
    /// leaf-complete mid, and identical retries CONVERGE.
    #[tokio::test]
    async fn padded_byte_flood_exhausts_typed_and_resumes() -> TestResult {
        use rio_store::test_helpers::proof_walk_with_caps;
        let s = StoreSession::new().await?;
        let mut minted = HashMap::new();
        let mut dc = HashMap::new();
        const PAD: usize = 1024 * 1024;
        let mids: Vec<String> = (0..8)
            .map(|i| {
                let leaf = mint_node(&format!("bfleaf{i}"), &[], &mut minted, &mut dc);
                mint_node_padded(&format!("bfmid{i}"), &[leaf], PAD, &mut minted, &mut dc)
            })
            .collect();
        let root = mint_node("bf-root", &mids, &mut minted, &mut dc);
        for (path, (text, _)) in &minted {
            stage_drv_sql(&s.db.pool, path, text).await?;
        }

        let arena_cap = 3 * PAD + PAD / 2; // 3.5 MiB: ~3 retained mids/attempt
        let mut attempts = 0;
        let mut last_rows = 0i64;
        loop {
            attempts += 1;
            let report = proof_walk_with_caps(
                &s.db.pool,
                None,
                &root,
                PROOF_WALK_WORK_MAX_FOR_TESTS,
                arena_cap,
            )
            .await?;
            let rows = cache_rows(&s.db.pool).await;
            if report.proven {
                break;
            }
            // Typed exhaustion, attributed to the BYTE dimension: the
            // work ledger is far from its cap when the byte ledger
            // refuses.
            assert_eq!(report.reason, Some("over_budget"), "{report:?}");
            assert!(
                report.work_used < PROOF_WALK_WORK_MAX_FOR_TESTS / 100,
                "work units must NOT be what bound this walk \
                 (got {} of {PROOF_WALK_WORK_MAX_FOR_TESTS})",
                report.work_used
            );
            assert!(
                rows > last_rows,
                "every byte-exhausted attempt must persist NEW progress \
                 (attempt {attempts}: {last_rows} -> {rows})"
            );
            last_rows = rows;
            assert!(attempts < 8, "must converge, not livelock");
        }
        assert_eq!(
            cache_rows(&s.db.pool).await,
            17,
            "8 leaves + 8 mids + root all proven"
        );
        assert!(
            attempts >= 2,
            "fixture must actually exercise byte-pressure resumption \
             (got {attempts} attempts)"
        );
        Ok(())
    }

    // r[verify store.put.ia-deriver-proof+4]
    /// Diamond dedup: A→{B,C}, B→D, C→D — D is fetched and probed ONCE.
    /// Exact op accounting (1 pre-admission probe + 1 post-admission
    /// probe + 4 fetches + 2 input probes = 8): a regression that
    /// re-queues D shows up as a count change, not a flake.
    #[tokio::test]
    async fn diamond_dedup() -> TestResult {
        let s = StoreSession::new().await?;
        let mut minted = HashMap::new();
        let mut dc = HashMap::new();
        let d = mint_node("dia-d", &[], &mut minted, &mut dc);
        let b = mint_node("dia-b", std::slice::from_ref(&d), &mut minted, &mut dc);
        let c = mint_node("dia-c", std::slice::from_ref(&d), &mut minted, &mut dc);
        let a = mint_node("dia-a", &[b, c], &mut minted, &mut dc);
        for (path, (text, _)) in &minted {
            stage_drv_sql(&s.db.pool, path, text).await?;
        }
        let report =
            proof_walk_for_tests(&s.db.pool, None, &a, PROOF_WALK_WORK_MAX_FOR_TESTS).await?;
        assert!(report.proven);
        assert_eq!(
            report.work_used, 8,
            "1 pre-admission probe + 1 post-admission probe + 4 fetches \
             + 2 input probes; dedup-on-push means D costs exactly once"
        );
        Ok(())
    }

    // r[verify store.put.ia-deriver-proof+4]
    /// A chunked `.drv` (≥256 KiB forces FastCDC chunking) is reassembled
    /// through the chunk cache and proves — chunked storage is no longer
    /// a verifiability boundary.
    #[tokio::test]
    async fn chunked_drv_proof_reassembles() -> TestResult {
        let (s, backend) = StoreSession::new_chunked().await?;
        // Big-but-valid .drv: padding via a huge env value.
        let pad = "p".repeat(400 * 1024);
        let mut minted = HashMap::new();
        let mut dc = HashMap::new();
        // Mint the shape first (small), then re-mint with padding folded
        // into the env so declared == derived still holds.
        let build = |out: &str| {
            format!(
                r#"Derive([("out","{out}","","")],[],[],"x86_64-linux","/bin/sh",["-c","x"],[("name","chunky"),("out","{out}"),("pad","{pad}")])"#
            )
        };
        let masked = Derivation::parse(&build("")).expect("masked parses");
        let name_only = format!("/nix/store/{}-chunky.drv", "a".repeat(32));
        let resolve = |_: &str| -> Option<&Derivation> { None };
        let paths =
            input_addressed_output_paths(&masked, &name_only, &resolve, &mut dc).expect("derives");
        let out = paths["out"].as_str().to_owned();
        let text = build(&out);
        let h = NixHash::new(HashAlgo::SHA256, Sha256::digest(text.as_bytes()).to_vec()).unwrap();
        let drv_path = StorePath::make_text("chunky.drv", &h, &[])
            .unwrap()
            .as_str()
            .to_owned();
        minted.insert(
            drv_path.clone(),
            (text.clone(), Derivation::parse(&text).unwrap()),
        );

        // Upload via gRPC so the store actually CHUNKS it.
        let node = rio_nix::nar::NarNode::Regular {
            executable: false,
            contents: text.into_bytes(),
        };
        let mut nar = Vec::new();
        rio_nix::nar::serialize(&mut nar, &node)?;
        let info = make_path_info_for_nar(&drv_path, &nar);
        let mut client = s.client.clone();
        assert!(put_path(&mut client, info, nar).await?);
        // Confirm it really is chunked, then evict the ingest-time cache
        // row to force the read-through to reassemble.
        let chunked: bool = sqlx::query_scalar(
            "SELECT m.inline_blob IS NULL FROM manifests m \
             JOIN narinfo n USING (store_path_hash) WHERE n.store_path = $1",
        )
        .bind(&drv_path)
        .fetch_one(&s.db.pool)
        .await?;
        assert!(chunked, "fixture must exercise the chunked manifest shape");
        sqlx::query("DELETE FROM drv_modulo_cache")
            .execute(&s.db.pool)
            .await?;

        let cache = std::sync::Arc::new(rio_store::cas::ChunkCache::new(std::sync::Arc::clone(
            &backend,
        )
            as std::sync::Arc<dyn ChunkBackend>));
        let report = proof_walk_for_tests(
            &s.db.pool,
            Some(&cache),
            &drv_path,
            PROOF_WALK_WORK_MAX_FOR_TESTS,
        )
        .await?;
        assert!(
            report.proven,
            "chunked .drv reassembles and proves: {report:?}"
        );
        assert!(
            report.work_used > 2,
            "chunk fetches are charged: {}",
            report.work_used
        );

        // bug_027 wire-class pin: the SAME chunked fixture walked
        // through an ERRORING backend (S3 outage class) must surface
        // the transient chunk-backend error — an infrastructure Err,
        // never an absence verdict, never the corruption/data-loss
        // class that pre-fix told operators bytes were lost.
        struct OutageBackend;
        #[async_trait::async_trait]
        impl ChunkBackend for OutageBackend {
            async fn put(&self, _h: &[u8; 32], _d: bytes::Bytes) -> anyhow::Result<()> {
                anyhow::bail!("put not under test")
            }
            async fn get(&self, _h: &[u8; 32]) -> anyhow::Result<Option<bytes::Bytes>> {
                anyhow::bail!("simulated S3 outage")
            }
            async fn exists_batch(&self, hashes: &[[u8; 32]]) -> anyhow::Result<Vec<bool>> {
                Ok(vec![false; hashes.len()])
            }
            fn key_for(&self, hash: &[u8; 32]) -> String {
                hex::encode(hash)
            }
            async fn delete_by_key(&self, _k: &str) -> anyhow::Result<()> {
                Ok(())
            }
        }
        sqlx::query("DELETE FROM drv_modulo_cache")
            .execute(&s.db.pool)
            .await?;
        let outage_cache = std::sync::Arc::new(rio_store::cas::ChunkCache::new(
            std::sync::Arc::new(OutageBackend) as std::sync::Arc<dyn ChunkBackend>,
        ));
        let err = proof_walk_for_tests(
            &s.db.pool,
            Some(&outage_cache),
            &drv_path,
            PROOF_WALK_WORK_MAX_FOR_TESTS,
        )
        .await
        .expect_err("backend outage must be an infrastructure Err, not a verdict");
        let msg = err.to_string();
        assert!(
            msg.contains("chunk backend unavailable"),
            "transient class carried (got: {msg})"
        );
        assert!(
            !msg.contains("data loss") && !msg.contains("corrupt"),
            "an S3 blip must not read as corruption/data loss (got: {msg})"
        );
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// C1c4: self-healing ingestion (order-independence by construction)
// ---------------------------------------------------------------------------

// r[verify store.ingest.drv-modulo-cache+2]
/// A reverse-topological batch (consumer FIRST, leaf LAST) populates
/// every row via the post-commit fixpoint — pre-fix only the leaf
/// populated and the rest waited for a proof-time read-through.
#[tokio::test]
async fn reverse_topological_batch_populates_all() -> TestResult {
    use rio_proto::types::{PutPathBatchRequest, PutPathRequest, put_path_request};
    let s = StoreSession::new_with_service_hmac(TEST_KEY.to_vec(), SERVICE_KEY.to_vec()).await?;

    // consumer -> multi -> leaf (golden corpus chain), uploaded in ONE
    // batch in REVERSE topological order.
    let entries: Vec<(&str, &str, Vec<&str>)> = vec![
        (
            GOLDEN_CONSUMER_PATH,
            GOLDEN_CONSUMER,
            vec![GOLDEN_FOD_PATH, GOLDEN_LEAF_PATH, GOLDEN_MULTI_PATH],
        ),
        (GOLDEN_MULTI_PATH, GOLDEN_MULTI, vec![]),
        (GOLDEN_FOD_PATH, GOLDEN_FOD, vec![]),
        (GOLDEN_LEAF_PATH, GOLDEN_LEAF, vec![]),
    ];

    let (tx, rx) = mpsc::channel(32);
    for (idx, (path, text, refs)) in entries.iter().enumerate() {
        let text = text.trim_end();
        let node = rio_nix::nar::NarNode::Regular {
            executable: false,
            contents: text.as_bytes().to_vec(),
        };
        let mut nar = Vec::new();
        rio_nix::nar::serialize(&mut nar, &node)?;
        let mut info = make_path_info_for_nar(path, &nar);
        info.references = refs
            .iter()
            .map(|r| rio_nix::store_path::StorePath::parse(r))
            .collect::<Result<_, _>>()?;
        let mut info: PathInfo = info.into();
        let trailer = rio_proto::types::PutPathTrailer {
            nar_hash: std::mem::take(&mut info.nar_hash),
            nar_size: std::mem::take(&mut info.nar_size),
        };
        for msg in [
            put_path_request::Msg::Metadata(rio_proto::types::PutPathMetadata { info: Some(info) }),
            put_path_request::Msg::NarChunk(nar),
            put_path_request::Msg::Trailer(trailer),
        ] {
            tx.send(PutPathBatchRequest {
                output_index: idx as u32,
                inner: Some(PutPathRequest { msg: Some(msg) }),
            })
            .await
            .unwrap();
        }
    }
    drop(tx);
    let mut req = tonic::Request::new(ReceiverStream::new(rx));
    req.metadata_mut().insert(
        rio_proto::SERVICE_TOKEN_HEADER,
        service_token().parse().unwrap(),
    );
    let mut client = s.client.clone();
    let resp = client.put_path_batch(req).await?.into_inner();
    assert_eq!(resp.created, vec![true, true, true, true]);

    for (path, _, _) in &entries {
        assert!(
            cache_row(&s.db.pool, path).await?.is_some(),
            "fixpoint populated {path} despite reverse-topological order"
        );
    }
    Ok(())
}

// r[verify store.ingest.drv-modulo-cache+2]
/// Re-upload of an already-complete `.drv` heals a missing cache row
/// (created=false; probe-first heal re-fires population).
#[tokio::test]
async fn already_complete_reupload_heals() -> TestResult {
    let mut s =
        StoreSession::new_with_service_hmac(TEST_KEY.to_vec(), SERVICE_KEY.to_vec()).await?;
    assert!(upload_drv_at(&mut s, GOLDEN_LEAF_PATH, GOLDEN_LEAF, &[]).await?);
    assert!(cache_row(&s.db.pool, GOLDEN_LEAF_PATH).await?.is_some());

    // Evict the row (simulates a row whose original population skipped).
    sqlx::query("DELETE FROM drv_modulo_cache")
        .execute(&s.db.pool)
        .await?;
    assert!(cache_row(&s.db.pool, GOLDEN_LEAF_PATH).await?.is_none());

    // Re-upload: created=false, heal re-populates (spawned — poll).
    assert!(!upload_drv_at(&mut s, GOLDEN_LEAF_PATH, GOLDEN_LEAF, &[]).await?);
    let mut tries = 0;
    loop {
        if cache_row(&s.db.pool, GOLDEN_LEAF_PATH).await?.is_some() {
            break;
        }
        tries += 1;
        assert!(tries < 100, "heal must repopulate the evicted row");
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    Ok(())
}

// r[verify store.ingest.drv-modulo-cache+2]
/// Probe-first: when the row already exists, the already-complete
/// re-upload changes nothing (immutable content-derived facts).
#[tokio::test]
async fn probe_first_heal_is_noop() -> TestResult {
    let mut s =
        StoreSession::new_with_service_hmac(TEST_KEY.to_vec(), SERVICE_KEY.to_vec()).await?;
    assert!(upload_drv_at(&mut s, GOLDEN_LEAF_PATH, GOLDEN_LEAF, &[]).await?);
    let before = cache_row(&s.db.pool, GOLDEN_LEAF_PATH)
        .await?
        .expect("populated");

    assert!(!upload_drv_at(&mut s, GOLDEN_LEAF_PATH, GOLDEN_LEAF, &[]).await?);
    // Give a hypothetical (buggy) rewrite a chance to land, then assert
    // byte-identical row.
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    let after = cache_row(&s.db.pool, GOLDEN_LEAF_PATH)
        .await?
        .expect("still present");
    assert_eq!(before.0, after.0, "modulo hash unchanged");
    assert_eq!(before.2, after.2, "deferred flag unchanged");
    let n: i64 = sqlx::query_scalar("SELECT count(*)::bigint FROM drv_modulo_cache")
        .fetch_one(&s.db.pool)
        .await?;
    assert_eq!(n, 1);
    Ok(())
}
