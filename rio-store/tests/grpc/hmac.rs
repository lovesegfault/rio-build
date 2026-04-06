//! PutPath HMAC assignment-token enforcement tests.
//!
//! Spec `sec.boundary.grpc-hmac`: workers must present a scheduler-
//! signed token proving the upload is for an assigned build. Without
//! this, a compromised worker could upload arbitrary paths.
//!
//! All tests use `StoreSession::new_with_hmac` so `hmac_verifier` is
//! Some — in dev mode (None) the check is bypassed entirely.

use super::*;
use rio_common::hmac::{AssignmentClaims, HmacSigner};
use std::time::{SystemTime, UNIX_EPOCH};

const TEST_KEY: &[u8] = b"test-hmac-key-at-least-32-bytes!!!";

fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs()
}

/// Build a valid Claims with the given expected_outputs and sign it.
fn sign_claims(outputs: Vec<String>, expiry_offset_secs: i64) -> String {
    let claims = AssignmentClaims {
        executor_id: "test-worker".into(),
        drv_hash: "0000000000000000000000000000000000000000000000000000000000000000".into(),
        expected_outputs: outputs,
        expiry_unix: (now_unix() as i64 + expiry_offset_secs) as u64,
        is_ca: false,
    };
    HmacSigner::from_key(TEST_KEY.to_vec()).sign(&claims)
}

// ---------------------------------------------------------------------------
// Enforcement ON + no token → reject
// ---------------------------------------------------------------------------

// r[verify sec.boundary.grpc-hmac]
#[tokio::test]
async fn hmac_no_token_rejected() -> TestResult {
    let mut s = StoreSession::new_with_hmac(TEST_KEY.to_vec()).await?;

    let path = test_store_path("hmac-no-token");
    let (nar, _) = make_nar(b"content");
    let info = make_path_info_for_nar(&path, &nar);

    // No token header → permission_denied.
    let err = put_path(&mut s.client, info, nar)
        .await
        .expect_err("no token → reject");
    assert_eq!(err.code(), tonic::Code::PermissionDenied);
    assert!(
        err.message().contains("token required"),
        "msg: {}",
        err.message()
    );

    Ok(())
}

// ---------------------------------------------------------------------------
// Valid token with path in expected_outputs → accept
// ---------------------------------------------------------------------------

#[tokio::test]
async fn hmac_valid_token_accepted() -> TestResult {
    let mut s = StoreSession::new_with_hmac(TEST_KEY.to_vec()).await?;

    let path = test_store_path("hmac-valid");
    let (nar, _) = make_nar(b"authorized upload");
    let info = make_path_info_for_nar(&path, &nar);

    // Token lists the exact path we're uploading.
    let token = sign_claims(vec![path.clone()], 60);

    let created = put_path_with_token(&mut s.client, info, nar, &token)
        .await
        .context("put with valid token")?;
    assert!(created, "valid token → accepted + created");

    Ok(())
}

// ---------------------------------------------------------------------------
// Invalid token (wrong key / garbage) → reject
// ---------------------------------------------------------------------------

#[tokio::test]
async fn hmac_invalid_token_rejected() -> TestResult {
    let mut s = StoreSession::new_with_hmac(TEST_KEY.to_vec()).await?;

    let path = test_store_path("hmac-invalid");
    let (nar, _) = make_nar(b"content");
    let info = make_path_info_for_nar(&path, &nar);

    // Garbage token — HmacVerifier::verify will fail.
    let err = put_path_with_token(&mut s.client, info, nar, "garbage-token")
        .await
        .expect_err("invalid token → reject");
    assert_eq!(err.code(), tonic::Code::PermissionDenied);
    assert!(
        err.message().contains("assignment token:"),
        "msg: {}",
        err.message()
    );

    Ok(())
}

#[tokio::test]
async fn hmac_wrong_key_signed_rejected() -> TestResult {
    let mut s = StoreSession::new_with_hmac(TEST_KEY.to_vec()).await?;

    let path = test_store_path("hmac-wrong-key");
    let (nar, _) = make_nar(b"content");
    let info = make_path_info_for_nar(&path, &nar);

    // Sign with a DIFFERENT key → MAC mismatch.
    let wrong_key = b"different-key-different-signature!!";
    let claims = AssignmentClaims {
        executor_id: "evil".into(),
        drv_hash: "00".repeat(32),
        expected_outputs: vec![path.clone()],
        expiry_unix: now_unix() + 60,
        is_ca: false,
    };
    let bad_token = HmacSigner::from_key(wrong_key.to_vec()).sign(&claims);

    let err = put_path_with_token(&mut s.client, info, nar, &bad_token)
        .await
        .expect_err("wrong-key token → reject");
    assert_eq!(err.code(), tonic::Code::PermissionDenied);

    Ok(())
}

// ---------------------------------------------------------------------------
// Expired token → reject (HmacVerifier checks expiry_unix)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn hmac_expired_token_rejected() -> TestResult {
    let mut s = StoreSession::new_with_hmac(TEST_KEY.to_vec()).await?;

    let path = test_store_path("hmac-expired");
    let (nar, _) = make_nar(b"content");
    let info = make_path_info_for_nar(&path, &nar);

    // Token expired 10 seconds ago.
    let token = sign_claims(vec![path.clone()], -10);

    let err = put_path_with_token(&mut s.client, info, nar, &token)
        .await
        .expect_err("expired → reject");
    assert_eq!(err.code(), tonic::Code::PermissionDenied);

    Ok(())
}

// ---------------------------------------------------------------------------
// Valid token but uploaded path NOT in expected_outputs → reject
// ---------------------------------------------------------------------------

#[tokio::test]
async fn hmac_path_not_in_claims_rejected() -> TestResult {
    let mut s = StoreSession::new_with_hmac(TEST_KEY.to_vec()).await?;

    let path = test_store_path("hmac-unauthorized-path");
    let (nar, _) = make_nar(b"content");
    let info = make_path_info_for_nar(&path, &nar);

    // Token authorizes a DIFFERENT path.
    let authorized = test_store_path("hmac-some-other-path");
    let token = sign_claims(vec![authorized], 60);

    let err = put_path_with_token(&mut s.client, info, nar, &token)
        .await
        .expect_err("path not in claims → reject");
    assert_eq!(err.code(), tonic::Code::PermissionDenied);
    assert!(
        err.message().contains("not authorized") || err.message().contains("not in"),
        "msg: {}",
        err.message()
    );

    Ok(())
}

// ---------------------------------------------------------------------------
// Sanity: verifier OFF (dev mode) → no enforcement
// ---------------------------------------------------------------------------

#[tokio::test]
async fn hmac_disabled_no_token_accepted() -> TestResult {
    // Normal session — no verifier.
    let mut s = StoreSession::new().await?;

    let path = test_store_path("hmac-dev-mode");
    let (nar, _) = make_nar(b"content");
    let info = make_path_info_for_nar(&path, &nar);

    // No token, no verifier → accepted (dev bypass).
    let created = put_path(&mut s.client, info, nar)
        .await
        .context("dev-mode put")?;
    assert!(created);

    Ok(())
}
