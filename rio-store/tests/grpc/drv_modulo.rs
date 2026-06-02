//! Store-side derivation modulo cache (`store.ingest.drv-modulo-cache`).
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

// r[verify store.ingest.drv-modulo-cache]
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

// r[verify store.ingest.drv-modulo-cache]
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

// r[verify store.ingest.drv-modulo-cache]
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

// r[verify store.ingest.drv-modulo-cache]
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

// r[verify store.ingest.drv-modulo-cache]
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

// r[verify store.put.ia-deriver-proof+2]
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

// r[verify store.put.ia-deriver-proof+2]
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
