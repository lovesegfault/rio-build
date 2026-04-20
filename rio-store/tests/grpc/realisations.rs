//! gRPC-level tests for RegisterRealisation, QueryRealisation.
//!
//! The underlying `realisations::*` are unit-tested directly; these
//! tests cover the gRPC wrapper's input validation branches (hash
//! length, empty output_name, missing fields).

use super::*;
use rio_proto::types::{QueryRealisationRequest, Realisation, RegisterRealisationRequest};

// ---------------------------------------------------------------------------
// RegisterRealisation — validation branches
// ---------------------------------------------------------------------------

fn valid_realisation() -> Realisation {
    Realisation {
        drv_hash: vec![0xAA; 32],
        output_name: "out".into(),
        output_path: test_store_path("ca-output"),
        output_hash: vec![0xBB; 32],
        signatures: vec![],
    }
}

#[tokio::test]
async fn register_realisation_missing_field_rejected() -> TestResult {
    let mut s = StoreSession::new().await?;

    let err = s
        .client
        .register_realisation(RegisterRealisationRequest { realisation: None })
        .await
        .expect_err("None realisation → invalid");
    assert_eq!(err.code(), tonic::Code::InvalidArgument);
    assert!(err.message().contains("required"), "msg: {}", err.message());

    Ok(())
}

#[tokio::test]
async fn register_realisation_bad_drv_hash_length_rejected() -> TestResult {
    let mut s = StoreSession::new().await?;

    let mut r = valid_realisation();
    r.drv_hash = vec![0xAA; 16]; // wrong length

    let err = s
        .client
        .register_realisation(RegisterRealisationRequest {
            realisation: Some(r),
        })
        .await
        .expect_err("short drv_hash → invalid");
    assert_eq!(err.code(), tonic::Code::InvalidArgument);

    Ok(())
}

#[tokio::test]
async fn register_realisation_bad_output_hash_length_rejected() -> TestResult {
    let mut s = StoreSession::new().await?;

    let mut r = valid_realisation();
    r.output_hash = vec![0xBB; 16];

    let err = s
        .client
        .register_realisation(RegisterRealisationRequest {
            realisation: Some(r),
        })
        .await
        .expect_err("short output_hash → invalid");
    assert_eq!(err.code(), tonic::Code::InvalidArgument);

    Ok(())
}

#[tokio::test]
async fn register_realisation_empty_output_name_rejected() -> TestResult {
    let mut s = StoreSession::new().await?;

    let mut r = valid_realisation();
    r.output_name = String::new();

    let err = s
        .client
        .register_realisation(RegisterRealisationRequest {
            realisation: Some(r),
        })
        .await
        .expect_err("empty output_name → invalid");
    assert_eq!(err.code(), tonic::Code::InvalidArgument);

    Ok(())
}

#[tokio::test]
async fn register_realisation_bad_output_path_rejected() -> TestResult {
    let mut s = StoreSession::new().await?;

    let mut r = valid_realisation();
    r.output_path = "not-a-store-path".into();

    let err = s
        .client
        .register_realisation(RegisterRealisationRequest {
            realisation: Some(r),
        })
        .await
        .expect_err("invalid output_path → invalid");
    assert_eq!(err.code(), tonic::Code::InvalidArgument);

    Ok(())
}

/// End-to-end: register → query roundtrip through gRPC.
#[tokio::test]
async fn register_then_query_realisation_roundtrip() -> TestResult {
    let mut s = StoreSession::new().await?;

    let r = valid_realisation();
    s.client
        .register_realisation(RegisterRealisationRequest {
            realisation: Some(r.clone()),
        })
        .await?;

    let got = s
        .client
        .query_realisation(QueryRealisationRequest {
            drv_hash: r.drv_hash.clone(),
            output_name: r.output_name.clone(),
        })
        .await?
        .into_inner();
    assert_eq!(got.output_path, r.output_path);
    assert_eq!(got.output_hash, r.output_hash);

    Ok(())
}

// ---------------------------------------------------------------------------
// QueryRealisation — validation branches
// ---------------------------------------------------------------------------

#[tokio::test]
async fn query_realisation_bad_drv_hash_length_rejected() -> TestResult {
    let mut s = StoreSession::new().await?;

    let err = s
        .client
        .query_realisation(QueryRealisationRequest {
            drv_hash: vec![0xAA; 16],
            output_name: "out".into(),
        })
        .await
        .expect_err("short drv_hash → invalid");
    assert_eq!(err.code(), tonic::Code::InvalidArgument);

    Ok(())
}

#[tokio::test]
async fn query_realisation_empty_output_name_rejected() -> TestResult {
    let mut s = StoreSession::new().await?;

    let err = s
        .client
        .query_realisation(QueryRealisationRequest {
            drv_hash: vec![0xAA; 32],
            output_name: String::new(),
        })
        .await
        .expect_err("empty output_name → invalid");
    assert_eq!(err.code(), tonic::Code::InvalidArgument);

    Ok(())
}

// ---------------------------------------------------------------------------
// RegisterRealisation — service-caller gate + conflict detection
// ---------------------------------------------------------------------------

const SERVICE_KEY: &[u8] = b"test-service-hmac-key-32-bytes!!!!";

fn sign_service(caller: &str) -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs();
    rio_auth::hmac::HmacSigner::from_key(SERVICE_KEY.to_vec()).sign(
        &rio_auth::hmac::ServiceClaims {
            caller: caller.into(),
            expiry_unix: now + 300,
        },
    )
}

fn register_req_with_service_token(
    r: Realisation,
    caller: &str,
) -> tonic::Request<RegisterRealisationRequest> {
    let mut req = tonic::Request::new(RegisterRealisationRequest {
        realisation: Some(r),
    });
    req.metadata_mut().insert(
        rio_proto::SERVICE_TOKEN_HEADER,
        sign_service(caller).parse().unwrap(),
    );
    req
}

// r[verify store.realisation.register+2]
/// With `service_verifier` configured (production), RegisterRealisation
/// without a service token → PermissionDenied. With a valid token from
/// an allowlisted caller → OK. The gateway no longer dispatches
/// `wopRegisterDrvOutput`, so this is the defense-in-depth layer.
#[tokio::test]
async fn register_realisation_requires_service_caller() -> TestResult {
    let mut s = StoreSession::new_with_service_hmac(
        b"test-hmac-key-at-least-32-bytes!!!".to_vec(),
        SERVICE_KEY.to_vec(),
    )
    .await?;

    // No token → PermissionDenied.
    let err = s
        .client
        .register_realisation(RegisterRealisationRequest {
            realisation: Some(valid_realisation()),
        })
        .await
        .expect_err("no service token → permission_denied");
    assert_eq!(err.code(), tonic::Code::PermissionDenied);
    assert!(
        err.message().contains("service token"),
        "msg: {}",
        err.message()
    );

    // Valid token from allowlisted caller (rio-scheduler) → OK.
    s.client
        .register_realisation(register_req_with_service_token(
            valid_realisation(),
            "rio-scheduler",
        ))
        .await?;

    // Wrong caller → PermissionDenied (not in service_bypass_callers).
    let mut r2 = valid_realisation();
    r2.drv_hash = vec![0xCC; 32];
    let err = s
        .client
        .register_realisation(register_req_with_service_token(r2, "rio-worker"))
        .await
        .expect_err("non-allowlisted caller → permission_denied");
    assert_eq!(err.code(), tonic::Code::PermissionDenied);

    Ok(())
}

// r[verify store.realisation.register+2]
/// Conflicting `output_path` for an existing `(drv_hash, output_name)`
/// → AlreadyExists, not silent ON CONFLICT DO NOTHING. Existing row
/// preserved (first-write-wins).
#[tokio::test]
async fn register_realisation_conflict_detected() -> TestResult {
    let mut s = StoreSession::new().await?; // dev mode (no service_verifier)

    let r = valid_realisation();
    s.client
        .register_realisation(RegisterRealisationRequest {
            realisation: Some(r.clone()),
        })
        .await?;

    let mut evil = r.clone();
    evil.output_path = test_store_path("ca-EVIL");
    let err = s
        .client
        .register_realisation(RegisterRealisationRequest {
            realisation: Some(evil.clone()),
        })
        .await
        .expect_err("different output_path → already_exists");
    assert_eq!(err.code(), tonic::Code::AlreadyExists);
    assert!(
        err.message().contains(&r.output_path) && err.message().contains(&evil.output_path),
        "error names both paths for ops triage: {}",
        err.message()
    );

    // First-write-wins — existing row unchanged.
    let got = s
        .client
        .query_realisation(QueryRealisationRequest {
            drv_hash: r.drv_hash.clone(),
            output_name: r.output_name.clone(),
        })
        .await?
        .into_inner();
    assert_eq!(got.output_path, r.output_path);

    Ok(())
}

#[tokio::test]
async fn query_realisation_not_found() -> TestResult {
    let mut s = StoreSession::new().await?;

    let err = s
        .client
        .query_realisation(QueryRealisationRequest {
            drv_hash: vec![0xEE; 32], // never registered
            output_name: "out".into(),
        })
        .await
        .expect_err("unknown → not_found");
    assert_eq!(err.code(), tonic::Code::NotFound);

    Ok(())
}
