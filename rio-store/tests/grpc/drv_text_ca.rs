//! `.drv` uploads are bound to their bytes (`store.put.drv-text-ca+2`).
//!
//! A derivation's store path is the text content-address of its
//! contents (`make_text(name, sha256(text), references)` — exactly how
//! nix's `writeDerivation` mints it). The store enforces that invariant
//! at every ingestion point, for every caller including the gateway's
//! service-token relays, so the `.drv` a gateway validated at
//! submission time is byte-identical to the one a worker later fetches
//! from the store.

use super::*;
use rio_auth::hmac::{HmacSigner, ServiceClaims};
use rio_test_support::fixtures::make_drv_nar;

const TEST_KEY: &[u8] = b"test-key-at-least-32-bytes-long!";
const SERVICE_KEY: &[u8] = b"test-service-hmac-key-32-bytes!!!!";

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

const DRV_TEXT_A: &[u8] =
    br#"Derive([("out","/nix/store/bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb-a-out","","")],[],[],"x86_64-linux","/bin/sh",[],[])"#;
const DRV_TEXT_B: &[u8] =
    br#"Derive([("out","/nix/store/cccccccccccccccccccccccccccccccc-b-out","","")],[],[],"x86_64-linux","/bin/sh",[],[])"#;

// r[verify store.put.drv-text-ca+2]
/// The honest flow: a `.drv` claimed at its canonical text-CA path is
/// accepted and registered (service-token relay, the gateway's shape).
#[tokio::test]
async fn drv_canonical_path_accepted() -> TestResult {
    let mut s =
        StoreSession::new_with_service_hmac(TEST_KEY.to_vec(), SERVICE_KEY.to_vec()).await?;
    let (path, nar) = make_drv_nar("honest-a.drv", DRV_TEXT_A, &[]);
    let info = make_path_info_for_nar(&path, &nar);

    let created = put_path_with_header(
        &mut s.client,
        info,
        nar,
        rio_proto::SERVICE_TOKEN_HEADER,
        &service_token(),
    )
    .await
    .context("canonical .drv upload")?;
    assert!(created);
    Ok(())
}

// r[verify store.put.drv-text-ca+2]
/// The same bytes claimed under a different `.drv` path are rejected —
/// even for trusted-plane (service-token) callers, since the binding is
/// what makes the cached/validated copy and the stored copy identical.
#[tokio::test]
async fn drv_wrong_path_rejected() -> TestResult {
    let mut s =
        StoreSession::new_with_service_hmac(TEST_KEY.to_vec(), SERVICE_KEY.to_vec()).await?;
    let (_path_a, nar_a) = make_drv_nar("victim-a.drv", DRV_TEXT_A, &[]);
    let (path_b, _nar_b) = make_drv_nar("victim-b.drv", DRV_TEXT_B, &[]);
    // Claim content A under B's canonical path.
    let info = make_path_info_for_nar(&path_b, &nar_a);

    let err = put_path_with_header(
        &mut s.client,
        info,
        nar_a,
        rio_proto::SERVICE_TOKEN_HEADER,
        &service_token(),
    )
    .await
    .expect_err(".drv content under a non-derived path must be rejected");
    assert_eq!(err.code(), tonic::Code::PermissionDenied);
    assert!(
        err.message().contains("text content-address"),
        "msg: {}",
        err.message()
    );
    Ok(())
}

// r[verify store.put.drv-text-ca+2]
/// The declared references are part of the text-CA derivation: claiming
/// a path minted with references while declaring none is rejected.
#[tokio::test]
async fn drv_wrong_references_rejected() -> TestResult {
    let mut s =
        StoreSession::new_with_service_hmac(TEST_KEY.to_vec(), SERVICE_KEY.to_vec()).await?;
    let dep = test_store_path("some-input-src");
    let (path, nar) = make_drv_nar("with-refs.drv", DRV_TEXT_A, &[dep.as_str()]);
    // Path embeds the reference; the upload declares none.
    let info = make_path_info_for_nar(&path, &nar);

    let err = put_path_with_header(
        &mut s.client,
        info,
        nar,
        rio_proto::SERVICE_TOKEN_HEADER,
        &service_token(),
    )
    .await
    .expect_err("reference set mismatch must be rejected");
    assert_eq!(err.code(), tonic::Code::PermissionDenied);
    Ok(())
}

// r[verify store.put.drv-text-ca+2]
/// The batch ingestion path enforces the same binding.
#[tokio::test]
async fn drv_batch_wrong_path_rejected() -> TestResult {
    use rio_proto::types::{PutPathBatchRequest, PutPathRequest, put_path_request};

    let s = StoreSession::new_with_service_hmac(TEST_KEY.to_vec(), SERVICE_KEY.to_vec()).await?;
    let mut client = s.client.clone();
    let (_path_a, nar_a) = make_drv_nar("batch-a.drv", DRV_TEXT_A, &[]);
    let (path_b, _nar_b) = make_drv_nar("batch-b.drv", DRV_TEXT_B, &[]);
    let mut info: PathInfo = make_path_info_for_nar(&path_b, &nar_a).into();
    let trailer = PutPathTrailer {
        nar_hash: std::mem::take(&mut info.nar_hash),
        nar_size: std::mem::take(&mut info.nar_size),
    };

    let (tx, rx) = mpsc::channel(8);
    let wrap = |m| PutPathBatchRequest {
        output_index: 0,
        inner: Some(PutPathRequest { msg: Some(m) }),
    };
    tx.send(wrap(put_path_request::Msg::Metadata(PutPathMetadata {
        info: Some(info),
    })))
    .await
    .unwrap();
    tx.send(wrap(put_path_request::Msg::NarChunk(nar_a)))
        .await
        .unwrap();
    tx.send(wrap(put_path_request::Msg::Trailer(trailer)))
        .await
        .unwrap();
    drop(tx);

    let mut req = tonic::Request::new(ReceiverStream::new(rx));
    req.metadata_mut().insert(
        rio_proto::SERVICE_TOKEN_HEADER,
        service_token().parse().unwrap(),
    );

    let err = client
        .put_path_batch(req)
        .await
        .expect_err(".drv batch entry under a non-derived path must be rejected");
    assert_eq!(err.code(), tonic::Code::PermissionDenied);
    assert!(
        err.message().contains("text content-address"),
        "msg: {}",
        err.message()
    );
    Ok(())
}

/// `store.put.idempotent` is unchanged: once the canonical `.drv` is
/// registered, a re-upload at the same (already-complete) path no-ops
/// with `created = false` — harmless, because the registered copy is
/// content-bound to the path.
#[tokio::test]
async fn drv_already_complete_stays_idempotent() -> TestResult {
    let mut s =
        StoreSession::new_with_service_hmac(TEST_KEY.to_vec(), SERVICE_KEY.to_vec()).await?;
    let (path, nar) = make_drv_nar("idempotent.drv", DRV_TEXT_A, &[]);

    let created = put_path_with_header(
        &mut s.client,
        make_path_info_for_nar(&path, &nar),
        nar.clone(),
        rio_proto::SERVICE_TOKEN_HEADER,
        &service_token(),
    )
    .await?;
    assert!(created);

    // Different bytes at the now-complete path: the idempotency
    // short-circuit answers before any content check runs.
    let (_other_path, other_nar) = make_drv_nar("other.drv", DRV_TEXT_B, &[]);
    let created = put_path_with_header(
        &mut s.client,
        make_path_info_for_nar(&path, &other_nar),
        other_nar,
        rio_proto::SERVICE_TOKEN_HEADER,
        &service_token(),
    )
    .await
    .context("re-upload at an already-complete path")?;
    assert!(!created, "already-complete path must no-op");
    Ok(())
}

// r[verify store.put.drv-text-ca+2]
/// The registered fail-closed divergence, end-to-end through the
/// service relay: a `.drv`-named path that is the legitimate SOURCE
/// content-address of its bytes (what `nix store add` of a `.drv` file
/// mints, accepted by CppNix's `registerValidPath`) is rejected by
/// rio's gate even for trusted-plane callers. Deliberately NO
/// differential-corpus entry — the corpus asserts parity; this pin and
/// the unit test are the divergence's record.
#[tokio::test]
async fn drv_named_source_ca_copy_rejected_via_relay() -> TestResult {
    use std::io::Write as _;
    let mut s =
        StoreSession::new_with_service_hmac(TEST_KEY.to_vec(), SERVICE_KEY.to_vec()).await?;

    let node = rio_nix::nar::NarNode::Regular {
        executable: false,
        contents: DRV_TEXT_A.to_vec(),
    };
    let mut nar = Vec::new();
    rio_nix::nar::serialize(&mut nar, &node)?;
    let mut w = rio_nix::ca::HashWriter::new(rio_nix::hash::HashAlgo::SHA256);
    w.write_all(&nar)?;
    let nar_hash = w.finish();
    let source_path = rio_nix::store_path::StorePath::make_fixed_output_with_self(
        "relay-copied.drv",
        &nar_hash,
        true,
        &[],
        false,
    )?;

    let info = make_path_info_for_nar(source_path.as_str(), &nar);
    let err = put_path_with_header(
        &mut s.client,
        info,
        nar,
        rio_proto::SERVICE_TOKEN_HEADER,
        &service_token(),
    )
    .await
    .expect_err("source-CA .drv copy must be rejected at the relay too");
    assert_eq!(err.code(), tonic::Code::PermissionDenied);
    assert!(
        err.message().contains("text content-address"),
        "msg: {}",
        err.message()
    );
    Ok(())
}
