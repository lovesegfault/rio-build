//! PutPath HMAC assignment-token enforcement tests.
//!
//! Spec `sec.boundary.grpc-hmac`: workers must present a scheduler-
//! signed token proving the upload is for an assigned build. Without
//! this, a compromised worker could upload arbitrary paths.
//!
//! Spec `store.put.builder-chunked-only`: a token that DOES verify is
//! a builder token (the scheduler only mints assignment tokens for
//! dispatched builds), and builders may not use the legacy buffered
//! RPCs at all — `PutPath`/`PutPathBatch` reject `role = Builder`
//! before reading the stream. Builder uploads (including the
//! floating-CA path-derivation and `expected_outputs` membership
//! checks) are exercised against `PutPathChunked` in
//! `tests/grpc/put_path_chunked.rs`; the legacy RPCs stay reachable
//! only for the gateway's service-token bypass and for dev mode.
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
    sign_claims_tenant(executor_id, outputs, is_ca, expiry_offset_secs, None)
}

fn sign_claims_tenant(
    executor_id: &str,
    outputs: Vec<String>,
    is_ca: bool,
    expiry_offset_secs: i64,
    tenant: Option<&str>,
) -> String {
    let claims = AssignmentClaims {
        executor_id: executor_id.into(),
        drv_hash: "0000000000000000000000000000000000000000000000000000000000000000".into(),
        expected_outputs: outputs,
        expiry_unix: (now_unix() as i64 + expiry_offset_secs) as u64,
        is_ca,
        tenant: tenant.map(String::from),
        role: rio_auth::hmac::TokenRole::Builder,
        input_closure_digest: String::new(),
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
// r[verify store.put.builder-chunked-only]
#[tokio::test]
async fn service_token_bypasses_hmac() -> TestResult {
    let mut s =
        StoreSession::new_with_service_hmac(TEST_KEY.to_vec(), SERVICE_KEY.to_vec()).await?;
    let path = test_store_path("svc-bypass");
    let (nar, _) = make_nar(b"content");
    let info = make_path_info_for_nar(&path, &nar);

    // Valid service token, NO assignment token → accepted (bypass).
    // This is also the gateway/admin half of builder-chunked-only:
    // the service-token path carries no role claim, so legacy PutPath
    // keeps working for `nix copy --to` while builders are rejected
    // (see hmac_builder_token_putpath_rejected).
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
// Builder-chunked-only gate: a VALID assignment token (role=Builder) is
// rejected on the legacy buffered RPCs — builders must use PutPathChunked.
// ---------------------------------------------------------------------------

/// A builder-role token — even one that is in-date and whose
/// `expected_outputs` lists exactly the uploaded path — may not use
/// legacy `PutPath`. The gate fires at the token-verify step, before
/// the stream is read, so no NAR bytes are buffered and no
/// `'uploading'` placeholder is ever claimed (the squat resistance
/// that bug_094 previously needed the CA-ordering fix for).
// r[verify store.put.builder-chunked-only]
#[tokio::test]
async fn hmac_builder_token_putpath_rejected() -> TestResult {
    let s = StoreSession::new_with_hmac(TEST_KEY.to_vec()).await?;
    let mut client = s.client.clone();

    let path = test_store_path("hmac-builder-legacy");
    let (nar, _) = make_nar(b"builder upload");
    let info = make_path_info_for_nar(&path, &nar);

    // Token lists the exact path we're uploading — rejection is about
    // the role, not token validity or path membership.
    let token = sign_claims(vec![path.clone()], 60);

    let err = put_path_with_token(&mut client, info, nar, &token)
        .await
        .expect_err("builder-role token on PutPath → reject");
    assert_eq!(err.code(), tonic::Code::PermissionDenied);
    assert!(
        err.message().contains("PutPathChunked"),
        "rejection should redirect builders to PutPathChunked: {}",
        err.message()
    );

    // Gate fires before any buffering or placeholder claim.
    let n: i64 = sqlx::query_scalar("SELECT count(*) FROM manifests")
        .fetch_one(&s.db.pool)
        .await?;
    assert_eq!(
        n, 0,
        "rejected builder PutPath must not claim a placeholder"
    );

    Ok(())
}

/// Same gate on the batch RPC.
// r[verify store.put.builder-chunked-only]
#[tokio::test]
async fn hmac_builder_token_putpathbatch_rejected() -> TestResult {
    use rio_proto::types::{PutPathBatchRequest, PutPathRequest, put_path_request};

    let s = StoreSession::new_with_hmac(TEST_KEY.to_vec()).await?;
    let mut client = s.client.clone();

    let path = test_store_path("hmac-builder-legacy-batch");
    let (nar, _) = make_nar(b"builder batch upload");
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
    let token = sign_claims(vec![path.clone()], 60);
    req.metadata_mut()
        .insert(rio_proto::ASSIGNMENT_TOKEN_HEADER, token.parse().unwrap());

    let err = client
        .put_path_batch(req)
        .await
        .expect_err("builder-role token on PutPathBatch → reject");
    assert_eq!(err.code(), tonic::Code::PermissionDenied);
    assert!(
        err.message().contains("PutPathChunked"),
        "rejection should redirect builders to PutPathChunked: {}",
        err.message()
    );

    let n: i64 = sqlx::query_scalar("SELECT count(*) FROM manifests")
        .fetch_one(&s.db.pool)
        .await?;
    assert_eq!(
        n, 0,
        "rejected builder PutPathBatch must not claim a placeholder"
    );

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
        tenant: None,
        role: rio_auth::hmac::TokenRole::Builder,
        input_closure_digest: String::new(),
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
        factor_json: r#"{"alu":1.5,"membw":1.0,"ioseq":1.0}"#.into(),
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

/// `submitting_tenant` is derived from `claims.tenant` (signed at
/// dispatch). The body field was REMOVED from the proto so it cannot be
/// supplied; this asserts the claims value reaches PG. A token without
/// a tenant claim → NULL.
// r[verify sched.sla.threat.hw-median-of-medians]
#[tokio::test]
async fn append_hw_perf_sample_tenant_from_claims() -> TestResult {
    let s = StoreSession::new_with_hmac(TEST_KEY.to_vec()).await?;
    let mut client = s.client.clone();
    // Token with tenant claim → that value is written.
    let token = sign_claims_tenant(
        "p-with-tenant",
        vec![String::new()],
        false,
        60,
        Some("tenant-uuid-a"),
    );
    append_hw(
        &mut client,
        "ignored",
        Some((rio_proto::ASSIGNMENT_TOKEN_HEADER, &token)),
    )
    .await
    .context("append with tenant claim")?;
    // Token without tenant claim (pre-tenant scheduler / orphan) → NULL.
    let token = sign_claims_tenant("p-no-tenant", vec![String::new()], false, 60, None);
    append_hw(
        &mut client,
        "ignored",
        Some((rio_proto::ASSIGNMENT_TOKEN_HEADER, &token)),
    )
    .await
    .context("append without tenant claim")?;

    let rows: Vec<(String, Option<String>)> = sqlx::query_as(
        "SELECT pod_id, submitting_tenant FROM hw_perf_samples \
         WHERE hw_class = 'aws-8-ebs-hi' ORDER BY pod_id",
    )
    .fetch_all(&s.db.pool)
    .await?;
    assert_eq!(
        rows,
        vec![
            ("p-no-tenant".into(), None),
            ("p-with-tenant".into(), Some("tenant-uuid-a".into())),
        ],
        "tenant from claims (signed), not body; absent claim → NULL"
    );
    Ok(())
}

/// `factor_json` is parsed, validated, then REBUILT from the present
/// scalars. Extra keys / padding in the body never reach PG — the
/// stored jsonb is exactly `{alu[, membw][, ioseq]}`. A 4MB body
/// padding would otherwise land verbatim in `hw_perf_samples.factor`.
// r[verify sched.sla.hw-bench-append-only]
#[tokio::test]
async fn append_hw_perf_sample_factor_json_extra_keys_stripped() -> TestResult {
    let s = StoreSession::new_with_hmac(TEST_KEY.to_vec()).await?;
    let mut client = s.client.clone();
    let token = sign_claims_full("p0", vec![String::new()], false, 60);
    let padding = "x".repeat(64 * 1024);
    let mut req = tonic::Request::new(AppendHwPerfSampleRequest {
        hw_class: "aws-8-ebs-hi".into(),
        pod_id: "ignored".into(),
        factor_json: format!(
            r#"{{"alu":1.5,"membw":1.0,"ioseq":1.0,"evil":"{padding}","z":[1,2,3]}}"#
        ),
    });
    req.metadata_mut()
        .insert(rio_proto::ASSIGNMENT_TOKEN_HEADER, token.parse().unwrap());
    client
        .append_hw_perf_sample(req)
        .await
        .context("append with padded factor_json")?;

    let stored: serde_json::Value =
        sqlx::query_scalar("SELECT factor FROM hw_perf_samples WHERE pod_id = 'p0'")
            .fetch_one(&s.db.pool)
            .await?;
    assert_eq!(
        stored,
        serde_json::json!({"alu": 1.5, "membw": 1.0, "ioseq": 1.0}),
        "stored jsonb rebuilt from validated scalars; extra keys dropped"
    );
    Ok(())
}

/// `hw_class` length-bounded at MAX_HW_CLASS_LEN. Unique key is
/// `(hw_class, pod_id)` — without a bound, one compromised builder with a
/// legitimate token could loop distinct multi-MB strings and fill
/// `hw_perf_samples` (M_041's "one row per pod start" assumed honest
/// callers). 65 chars → InvalidArgument; nothing inserted.
// r[verify sched.sla.hw-bench-append-only]
#[tokio::test]
async fn append_hw_perf_sample_oversized_hw_class_rejected() -> TestResult {
    let s = StoreSession::new_with_hmac(TEST_KEY.to_vec()).await?;
    let mut client = s.client.clone();
    let token = sign_claims_full("real-pod", vec![String::new()], false, 60);
    let mut req = tonic::Request::new(AppendHwPerfSampleRequest {
        hw_class: "a".repeat(rio_common::limits::MAX_HW_CLASS_LEN + 1),
        pod_id: "ignored".into(),
        factor_json: r#"{"alu":1.5,"membw":1.0,"ioseq":1.0}"#.into(),
    });
    req.metadata_mut()
        .insert(rio_proto::ASSIGNMENT_TOKEN_HEADER, token.parse().unwrap());
    let err = client
        .append_hw_perf_sample(req)
        .await
        .expect_err("oversized hw_class → reject");
    assert_eq!(err.code(), tonic::Code::InvalidArgument);
    let n: i64 = sqlx::query_scalar("SELECT count(*) FROM hw_perf_samples")
        .fetch_one(&s.db.pool)
        .await?;
    assert_eq!(n, 0, "rejected request must not INSERT");
    Ok(())
}

/// `hw_class` charset-bounded to `[a-z0-9-]` — the controller-stamped
/// 4-segment format. Slash, uppercase, unicode → InvalidArgument.
// r[verify sched.sla.hw-bench-append-only]
#[tokio::test]
async fn append_hw_perf_sample_bad_charset_rejected() -> TestResult {
    let s = StoreSession::new_with_hmac(TEST_KEY.to_vec()).await?;
    let mut client = s.client.clone();
    let token = sign_claims_full("real-pod", vec![String::new()], false, 60);
    for bad in ["aws/7/ebs", "AWS-7-ebs-hi", "aws-7-ébs-hi"] {
        let mut req = tonic::Request::new(AppendHwPerfSampleRequest {
            hw_class: bad.into(),
            pod_id: "ignored".into(),
            factor_json: r#"{"alu":1.5,"membw":1.0,"ioseq":1.0}"#.into(),
        });
        req.metadata_mut()
            .insert(rio_proto::ASSIGNMENT_TOKEN_HEADER, token.parse().unwrap());
        let err = client
            .append_hw_perf_sample(req)
            .await
            .expect_err("bad charset → reject");
        assert_eq!(err.code(), tonic::Code::InvalidArgument, "hw_class={bad:?}");
    }
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

// ---------------------------------------------------------------------------
// store_path_hash forgery — server MUST recompute (sec.boundary.grpc-hmac)
// ---------------------------------------------------------------------------

/// bug_068: HMAC binds `store_path` (string), not `store_path_hash`.
/// A caller uploading path A sends `{store_path: A, store_path_hash:
/// sha256(B)}`; pre-fix, the server keyed A's narinfo under B's slot —
/// poisoning B's hash lookups. Post-fix, `validate_put_metadata`
/// recomputes `store_path_hash` from the gated `store_path`
/// unconditionally; the wire value is ignored.
///
/// Exercised via the gateway's service-token bypass: since
/// `store.put.builder-chunked-only` an assignment (builder) token can
/// no longer reach legacy `PutPath` at all, so the gateway is the
/// caller that still hits this recompute (PutPathChunked never reads a
/// wire `store_path_hash` — the chunked handler derives it
/// server-side).
// r[verify sec.boundary.grpc-hmac]
#[tokio::test]
async fn hmac_store_path_hash_mismatch_ignored() -> TestResult {
    use sha2::{Digest, Sha256};

    let mut s =
        StoreSession::new_with_service_hmac(TEST_KEY.to_vec(), SERVICE_KEY.to_vec()).await?;

    let path_a = test_store_path("hmac-hashforge-a");
    let path_b = test_store_path("hmac-hashforge-b");
    let (nar, _) = make_nar(b"authorized payload for A");
    let info = make_path_info_for_nar(&path_a, &nar);

    // Forge: store_path=A but store_path_hash=sha256(B).
    let mut raw: PathInfo = info.into();
    raw.store_path_hash = Sha256::digest(path_b.as_bytes()).to_vec();
    let trailer = PutPathTrailer {
        nar_hash: std::mem::take(&mut raw.nar_hash),
        nar_size: std::mem::take(&mut raw.nar_size),
    };

    let (tx, rx) = mpsc::channel(8);
    tx.send(PutPathRequest {
        msg: Some(put_path_request::Msg::Metadata(PutPathMetadata {
            info: Some(raw),
        })),
    })
    .await
    .unwrap();
    tx.send(PutPathRequest {
        msg: Some(put_path_request::Msg::NarChunk(nar)),
    })
    .await
    .unwrap();
    tx.send(PutPathRequest {
        msg: Some(put_path_request::Msg::Trailer(trailer)),
    })
    .await
    .unwrap();
    drop(tx);

    let mut req = tonic::Request::new(ReceiverStream::new(rx));
    req.metadata_mut().insert(
        rio_proto::SERVICE_TOKEN_HEADER,
        sign_service("rio-gateway", 60)
            .parse()
            .expect("ascii token"),
    );
    let created = s.client.put_path(req).await?.into_inner().created;
    assert!(created, "upload of A with forged hash succeeds");

    // B's slot MUST be untouched.
    let b_result = s
        .client
        .query_path_info(QueryPathInfoRequest {
            store_path: path_b.clone(),
        })
        .await;
    assert_eq!(
        b_result.expect_err("B was never uploaded").code(),
        tonic::Code::NotFound,
        "forged store_path_hash must NOT key A's narinfo under B's slot"
    );

    // A's slot MUST hold the upload (server-derived hash).
    let a_info = s
        .client
        .query_path_info(QueryPathInfoRequest {
            store_path: path_a.clone(),
        })
        .await?
        .into_inner();
    assert_eq!(
        a_info.store_path, path_a,
        "A keyed under its own server-derived hash"
    );

    Ok(())
}
