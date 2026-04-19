//! PutPath HMAC assignment-token enforcement tests.
//!
//! Spec `sec.boundary.grpc-hmac`: workers must present a scheduler-
//! signed token proving the upload is for an assigned build. Without
//! this, a compromised worker could upload arbitrary paths.
//!
//! All tests use `StoreSession::new_with_hmac` so `hmac_verifier` is
//! Some — in dev mode (None) the check is bypassed entirely.

use super::*;
use rio_auth::hmac::{AssignmentClaims, HmacSigner, ServiceClaims};
use std::time::{SystemTime, UNIX_EPOCH};

const TEST_KEY: &[u8] = b"test-hmac-key-at-least-32-bytes!!!";
const SERVICE_KEY: &[u8] = b"test-service-hmac-key-32-bytes!!!!";

fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs()
}

/// Build a valid Claims with the given expected_outputs and sign it.
fn sign_claims(outputs: Vec<String>, expiry_offset_secs: i64) -> String {
    sign_claims_full("test-worker", outputs, false, expiry_offset_secs)
}

fn sign_claims_full(
    executor_id: &str,
    outputs: Vec<String>,
    is_ca: bool,
    expiry_offset_secs: i64,
) -> String {
    let claims = AssignmentClaims {
        executor_id: executor_id.into(),
        drv_hash: "0000000000000000000000000000000000000000000000000000000000000000".into(),
        expected_outputs: outputs,
        expiry_unix: (now_unix() as i64 + expiry_offset_secs) as u64,
        is_ca,
    };
    HmacSigner::from_key(TEST_KEY.to_vec()).sign(&claims)
}

// ---------------------------------------------------------------------------
// Enforcement ON + no token → reject
// ---------------------------------------------------------------------------

fn sign_service(caller: &str, expiry_offset_secs: i64) -> String {
    let claims = ServiceClaims {
        caller: caller.into(),
        expiry_unix: (now_unix() as i64 + expiry_offset_secs) as u64,
    };
    HmacSigner::from_key(SERVICE_KEY.to_vec()).sign(&claims)
}

// ---------------------------------------------------------------------------
// Service-token bypass (transport-agnostic CN-allowlist replacement)
// ---------------------------------------------------------------------------

// r[verify sec.authz.service-token]
#[tokio::test]
async fn service_token_bypasses_hmac() -> TestResult {
    let mut s =
        StoreSession::new_with_service_hmac(TEST_KEY.to_vec(), SERVICE_KEY.to_vec()).await?;
    let path = test_store_path("svc-bypass");
    let (nar, _) = make_nar(b"content");
    let info = make_path_info_for_nar(&path, &nar);

    // Valid service token, NO assignment token → accepted (bypass).
    let created = put_path_with_header(
        &mut s.client,
        info,
        nar,
        rio_proto::SERVICE_TOKEN_HEADER,
        &sign_service("rio-gateway", 60),
    )
    .await?;
    assert!(created);
    Ok(())
}

#[tokio::test]
async fn expired_service_token_rejected() -> TestResult {
    let mut s =
        StoreSession::new_with_service_hmac(TEST_KEY.to_vec(), SERVICE_KEY.to_vec()).await?;
    let path = test_store_path("svc-expired");
    let (nar, _) = make_nar(b"content");
    let info = make_path_info_for_nar(&path, &nar);

    let err = put_path_with_header(
        &mut s.client,
        info,
        nar,
        rio_proto::SERVICE_TOKEN_HEADER,
        &sign_service("rio-gateway", -60),
    )
    .await
    .expect_err("expired service token should be rejected");
    assert_eq!(err.code(), tonic::Code::PermissionDenied);
    Ok(())
}

#[tokio::test]
async fn service_token_wrong_caller_rejected() -> TestResult {
    let mut s =
        StoreSession::new_with_service_hmac(TEST_KEY.to_vec(), SERVICE_KEY.to_vec()).await?;
    let path = test_store_path("svc-wrong-caller");
    let (nar, _) = make_nar(b"content");
    let info = make_path_info_for_nar(&path, &nar);

    // Valid signature but caller not in allowlist → reject. Proves a
    // compromised builder that somehow obtained the service key still
    // cannot bypass without ALSO knowing an allowlisted caller name
    // (defense-in-depth; the primary defense is key isolation).
    let err = put_path_with_header(
        &mut s.client,
        info,
        nar,
        rio_proto::SERVICE_TOKEN_HEADER,
        &sign_service("rio-builder", 60),
    )
    .await
    .expect_err("non-allowlisted caller should be rejected");
    assert_eq!(err.code(), tonic::Code::PermissionDenied);
    assert!(err.message().contains("not in allowlist"));
    Ok(())
}

#[tokio::test]
async fn service_token_wrong_key_rejected() -> TestResult {
    let mut s =
        StoreSession::new_with_service_hmac(TEST_KEY.to_vec(), SERVICE_KEY.to_vec()).await?;
    let path = test_store_path("svc-wrong-key");
    let (nar, _) = make_nar(b"content");
    let info = make_path_info_for_nar(&path, &nar);

    // Signed with the ASSIGNMENT key (which a builder might plausibly
    // obtain via a leaked WorkAssignment) → service_verifier rejects
    // (different key → InvalidSignature). This is the core threat-
    // model property: assignment-key compromise does NOT grant bypass.
    let forged = HmacSigner::from_key(TEST_KEY.to_vec()).sign(&ServiceClaims {
        caller: "rio-gateway".into(),
        expiry_unix: now_unix() + 60,
    });
    let err = put_path_with_header(
        &mut s.client,
        info,
        nar,
        rio_proto::SERVICE_TOKEN_HEADER,
        &forged,
    )
    .await
    .expect_err("wrong-key service token should be rejected");
    assert_eq!(err.code(), tonic::Code::PermissionDenied);
    Ok(())
}

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

// ---------------------------------------------------------------------------
// Floating-CA: store_path derived server-side from verified nar_hash
// ---------------------------------------------------------------------------

/// Compute the floating-CA store path for `nar` the way the server
/// does (`make_fixed_output(name, sha256(nar), recursive, [])`).
fn ca_path_for(name: &str, nar: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    let h = rio_nix::hash::NixHash::new(
        rio_nix::hash::HashAlgo::SHA256,
        Sha256::digest(nar).to_vec(),
    )
    .unwrap();
    rio_nix::store_path::StorePath::make_fixed_output(name, &h, true, &[])
        .unwrap()
        .to_string()
}

// r[verify sec.authz.ca-path-derived]
#[tokio::test]
async fn hmac_is_ca_correct_path_accepted() -> TestResult {
    let mut s = StoreSession::new_with_hmac(TEST_KEY.to_vec()).await?;
    let (nar, _) = make_nar(b"ca content");
    let path = ca_path_for("ca-ok", &nar);
    let info = make_path_info_for_nar(&path, &nar);

    // is_ca=true, expected_outputs empty (as at dispatch time). Upload
    // to the content-derived path → accepted.
    let token = sign_claims_full("test-worker", vec![String::new()], true, 60);
    let created = put_path_with_token(&mut s.client, info, nar, &token)
        .await
        .context("put to derived CA path")?;
    assert!(created);
    Ok(())
}

// r[verify sec.authz.ca-path-derived]
#[tokio::test]
async fn hmac_is_ca_wrong_path_rejected() -> TestResult {
    let mut s = StoreSession::new_with_hmac(TEST_KEY.to_vec()).await?;
    let (nar, _) = make_nar(b"evil content");
    // Upload to an ARBITRARY (IA-shaped) path that is NOT the
    // content-derived CA path. Pre-fix this was accepted — the
    // "backdoored libc" scenario.
    let path = test_store_path("evil-glibc-2.38");
    let info = make_path_info_for_nar(&path, &nar);

    let token = sign_claims_full("test-worker", vec![String::new()], true, 60);
    let err = put_path_with_token(&mut s.client, info, nar, &token)
        .await
        .expect_err("is_ca token to non-derived path → reject");
    assert_eq!(err.code(), tonic::Code::PermissionDenied);
    assert!(
        err.message().contains("content-derived CA path"),
        "msg: {}",
        err.message()
    );
    Ok(())
}

#[tokio::test]
async fn hmac_is_ca_wrong_hash_part_rejected() -> TestResult {
    let mut s = StoreSession::new_with_hmac(TEST_KEY.to_vec()).await?;
    let (nar, _) = make_nar(b"ca content 2");
    // Correct name, WRONG hash-part (test_store_path uses a fixed
    // TEST_HASH). Proves the check compares the full path, not just
    // the name.
    let derived = ca_path_for("ca-hashpart", &nar);
    let wrong = test_store_path("ca-hashpart");
    assert_ne!(derived, wrong, "test precondition: paths differ");
    let info = make_path_info_for_nar(&wrong, &nar);

    let token = sign_claims_full("test-worker", vec![String::new()], true, 60);
    let err = put_path_with_token(&mut s.client, info, nar, &token)
        .await
        .expect_err("wrong hash-part → reject");
    assert_eq!(err.code(), tonic::Code::PermissionDenied);
    Ok(())
}

// r[verify sec.authz.ca-path-derived]
/// `PutPathBatch` is the multi-output endpoint builders use; the CA
/// path-derivation gate must apply there too. Same attack as
/// [`hmac_is_ca_wrong_path_rejected`] but via the batch RPC.
#[tokio::test]
async fn hmac_is_ca_batch_wrong_path_rejected() -> TestResult {
    use rio_proto::types::{PutPathBatchRequest, PutPathRequest, put_path_request};

    let s = StoreSession::new_with_hmac(TEST_KEY.to_vec()).await?;
    let mut client = s.client.clone();
    let (nar, _) = make_nar(b"evil batch content");
    let path = test_store_path("evil-batch-target");
    let mut info: PathInfo = make_path_info_for_nar(&path, &nar).into();
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
    tx.send(wrap(put_path_request::Msg::NarChunk(nar)))
        .await
        .unwrap();
    tx.send(wrap(put_path_request::Msg::Trailer(trailer)))
        .await
        .unwrap();
    drop(tx);

    let mut req = tonic::Request::new(ReceiverStream::new(rx));
    let token = sign_claims_full("test-worker", vec![String::new()], true, 60);
    req.metadata_mut()
        .insert(rio_proto::ASSIGNMENT_TOKEN_HEADER, token.parse().unwrap());

    let err = client
        .put_path_batch(req)
        .await
        .expect_err("is_ca batch to non-derived path → reject");
    assert_eq!(err.code(), tonic::Code::PermissionDenied);
    assert!(
        err.message().contains("content-derived CA path"),
        "msg: {}",
        err.message()
    );
    Ok(())
}

// ---------------------------------------------------------------------------
// AppendHwPerfSample: pod_id derived from claims, not body
// ---------------------------------------------------------------------------

use rio_proto::types::AppendHwPerfSampleRequest;

async fn append_hw(
    client: &mut StoreServiceClient<Channel>,
    body_pod_id: &str,
    header: Option<(&'static str, &str)>,
) -> Result<(), tonic::Status> {
    let mut req = tonic::Request::new(AppendHwPerfSampleRequest {
        hw_class: "aws-8-ebs-hi".into(),
        pod_id: body_pod_id.into(),
        factor: 1.5,
    });
    if let Some((h, v)) = header {
        req.metadata_mut().insert(h, v.parse().unwrap());
    }
    client.append_hw_perf_sample(req).await.map(|_| ())
}

// r[verify sec.boundary.grpc-hmac]
#[tokio::test]
async fn append_hw_perf_sample_without_token_rejected() -> TestResult {
    let mut s = StoreSession::new_with_hmac(TEST_KEY.to_vec()).await?;
    let err = append_hw(&mut s.client, "fake-pod", None)
        .await
        .expect_err("no token → reject");
    assert_eq!(err.code(), tonic::Code::PermissionDenied);
    Ok(())
}

// r[verify sec.boundary.grpc-hmac]
#[tokio::test]
async fn append_hw_perf_sample_forged_pod_id_ignored() -> TestResult {
    let s = StoreSession::new_with_hmac(TEST_KEY.to_vec()).await?;
    let mut client = s.client.clone();
    // Token signed for executor_id="real-pod"; body says "fake-pod".
    // Server MUST write pod_id='real-pod'.
    let token = sign_claims_full("real-pod", vec![String::new()], false, 60);
    append_hw(
        &mut client,
        "fake-pod",
        Some((rio_proto::ASSIGNMENT_TOKEN_HEADER, &token)),
    )
    .await
    .context("append with valid token")?;

    let row: (String,) =
        sqlx::query_as("SELECT pod_id FROM hw_perf_samples WHERE hw_class = 'aws-8-ebs-hi'")
            .fetch_one(&s.db.pool)
            .await?;
    assert_eq!(
        row.0, "real-pod",
        "pod_id from claims.executor_id, not body"
    );
    Ok(())
}

#[tokio::test]
async fn append_hw_perf_sample_service_token_rejected() -> TestResult {
    let mut s =
        StoreSession::new_with_service_hmac(TEST_KEY.to_vec(), SERVICE_KEY.to_vec()).await?;
    // Gateway has no business writing hw_perf_samples; service-token
    // bypass yields no executor_id → reject.
    let err = append_hw(
        &mut s.client,
        "fake-pod",
        Some((
            rio_proto::SERVICE_TOKEN_HEADER,
            &sign_service("rio-gateway", 60),
        )),
    )
    .await
    .expect_err("service token → reject");
    assert_eq!(err.code(), tonic::Code::PermissionDenied);
    assert!(
        err.message().contains("service-token"),
        "msg: {}",
        err.message()
    );
    Ok(())
}
